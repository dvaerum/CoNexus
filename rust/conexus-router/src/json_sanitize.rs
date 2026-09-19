//! The one entry point for decoding an untrusted `/api` REST request
//! body. Port of `conexus/utils/json_utils.py`'s
//! `decode_untrusted_body` chokepoint -- eleven pentest findings
//! across nine rounds (R3-F1, R4-F3, R4-F4, R5-F8, R5-F9, R13-F2,
//! R14-F3, R15-F1, R15-F2, R16-F3, R20-F2) were all variations of
//! "some decode point skipped sanitization"; this is the ONE seam
//! every mutating `/api` handler decodes through, matching Python's
//! own "one chokepoint, not eleven copies" design.
//!
//! **Deliberate simplification, not a silent gap**: Python's
//! `sanitize_json_input` attempts a multi-step "aggressive" recovery
//! of MALFORMED JSON via regex-based whitespace/structure repair
//! before giving up. No real client (the dashboard always sends
//! `JSON.stringify`'d well-formed bodies) depends on that recovery
//! path -- it exists for historical tolerance of hand-crafted/buggy
//! callers, not a documented contract. This port rejects malformed
//! JSON outright (400) rather than attempting string-surgery
//! recovery, preserving every SECURITY property (control-byte/hidden-
//! Unicode stripping, the depth guard, the top-level-object guard,
//! client-safe error messages) while dropping only the best-effort
//! leniency.
//!
//! The depth guard is not a mere nicety here the way it barely was in
//! Python (`RecursionError` is catchable there): `serde_json`'s
//! recursive-descent parser can trigger a genuine native stack
//! overflow on deeply-nested input, which ABORTS THE WHOLE PROCESS --
//! taking every live connection on this project's backend down, not
//! just failing the one request. The pre-scan below rejects excessive
//! nesting from the raw bytes BEFORE `serde_json` ever parses them.
//!
//! **Duplicated verbatim from `conexus-backend::json_sanitize`
//! (Phase E1 PR 851's own port), not shared via a new crate** --
//! `conexus-backend`/`conexus-router` are deliberately separate
//! binary crates in the target architecture (ADR-0020: the router
//! never depends on anything backend-specific) that can't depend on
//! each other, and this migration already made the identical call for
//! `sanitize_string_leaf` itself (which moved to `conexus-core`
//! precisely so BOTH binaries could share the character-class logic
//! without a body-decode-shaped crate existing for it). Introducing a
//! new shared library crate this late in the migration, for one
//! function, would be a bigger architectural change than this PR's
//! own scope -- see the PR23 app-wiring research's own explicit
//! resolution of this exact question. The router's own admin-REST
//! handlers (`admin_users_users.rs`/`admin_groups.rs`/etc.) are this
//! module's first real consumers, via `perm_gates.rs`'s
//! `read_body_and_revalidate` fusion wrapper.

use serde_json::{Map, Value};

/// Matches Python's `PF-R20-1`/`R20-F3` recursion guard intent (chosen
/// independently for Rust's stack, not copied from a Python constant
/// -- CPython's default recursion limit and Rust's native stack depth
/// per JSON nesting level are not the same budget).
///
/// F16: pinned to `serde_json` 1.0.151's (the version in `Cargo.lock`)
/// own empirically-measured internal recursion limit for `Value`
/// deserialization -- 127 levels succeed, 128 fails with "recursion
/// limit exceeded" (measured directly against `serde_json::from_str`,
/// the exact call this module makes below; see
/// `serde_json_recursion_limit_matches_the_boundary_this_module_assumes`
/// for the reproducible probe). The constant used to be a looser 200,
/// which let a body in the 127-199 range sail past this pre-scan and
/// get rejected by `serde_json` itself with the wrong error message
/// ("not valid JSON" instead of "too deeply nested") -- this module's
/// OWN guard must be the one that fires first, at the exact boundary
/// `serde_json` actually enforces, both for the correct message AND so
/// the "reject before an unbounded-depth parse could abort the
/// process" safety property this module exists for is guaranteed by
/// this code, not by an accident of the currently-pinned dependency
/// version. The canary test below fails loudly if a future
/// `serde_json` upgrade moves this boundary.
const MAX_NESTING_DEPTH: usize = 127;

/// Client-safe error from [`decode_untrusted_body`]. `Display` is
/// deliberately the only way to extract text -- see Python's
/// `UntrustedBodyError` docstring: this string is returned to the
/// client verbatim (`{"error": ...}`), so it must never carry
/// interpreter/library internals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrustedBodyError(String);

impl std::fmt::Display for UntrustedBodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for UntrustedBodyError {}

// -- Character-class sanitization (R4-F3/R5-F8/R13-F2/R14-F3/R15-F1) --
//
// The actual per-string-leaf stripper moved to
// `conexus_core::string_sanitize::sanitize_string_leaf` (Phase E2 PR 3)
// so `conexus-router::identity`'s `create_user` (which needs the
// IDENTICAL stripper for `username`/`email`) can share it instead of
// carrying a second, drift-risking copy -- `conexus-backend` and
// `conexus-router` are separate binary crates that can't depend on
// each other, so the shared logic lives in the one crate both already
// depend on. See that function's own doc for the full character-class
// rationale (control bytes, hidden-format Unicode, the Cs/surrogate
// structural-impossibility note, the combining-mark run cap).

/// Recursively walk a parsed JSON value, sanitizing every string leaf.
/// Dict/object keys are left untouched (matches Python's historical
/// behavior -- only values were ever sanitized).
fn strip_control_bytes(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(conexus_core::string_sanitize::sanitize_string_leaf(&s)),
        Value::Array(items) => Value::Array(items.into_iter().map(strip_control_bytes).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, strip_control_bytes(v)))
                .collect(),
        ),
        other => other,
    }
}

/// Pre-scan the raw bytes for bracket nesting depth, without fully
/// parsing -- rejects pathological nesting before `serde_json` ever
/// sees it (see the module doc for why this matters more in Rust than
/// it did in Python). A lightweight bracket-depth counter that
/// respects string literals (so a `{`/`[` inside a quoted string
/// doesn't count) is sufficient here: it only needs to catch genuinely
/// deep nesting, not validate JSON syntax -- `serde_json::from_str`
/// still does the real parse and is the actual syntax authority.
fn exceeds_max_nesting_depth(raw: &str) -> bool {
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;
    for ch in raw.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' | '[' => {
                depth += 1;
                if depth > MAX_NESTING_DEPTH {
                    return true;
                }
            }
            '}' | ']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

/// Decode `raw` (a request body) into a sanitized top-level JSON
/// object. Port of `decode_untrusted_body`, narrowed per the module
/// doc's deliberate-simplification note (well-formed JSON only, no
/// aggressive malformed-JSON recovery).
///
/// Rejects: invalid UTF-8, a body deep enough to trip
/// [`MAX_NESTING_DEPTH`], malformed JSON, and a top-level value that
/// isn't a JSON object (PF-R12-1 -- every real caller immediately does
/// field-lookups that assume an object).
pub fn decode_untrusted_body(raw: &[u8]) -> Result<Map<String, Value>, UntrustedBodyError> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| UntrustedBodyError("request body is not valid UTF-8".to_string()))?;

    if exceeds_max_nesting_depth(text) {
        return Err(UntrustedBodyError(
            "request body is too deeply nested".to_string(),
        ));
    }

    let parsed: Value = serde_json::from_str(text)
        .map_err(|_| UntrustedBodyError("request body is not valid JSON".to_string()))?;

    match strip_control_bytes(parsed) {
        Value::Object(map) => Ok(map),
        _ => Err(UntrustedBodyError(
            "request body must be a JSON object".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_object_round_trips_unchanged() {
        let body = br#"{"a": "hello", "b": 42, "c": true}"#;
        let decoded = decode_untrusted_body(body).unwrap();
        assert_eq!(decoded["a"], "hello");
        assert_eq!(decoded["b"], 42);
        assert_eq!(decoded["c"], true);
    }

    #[test]
    fn control_bytes_are_stripped_from_string_leaves() {
        // U+0000-U+001F must be JSON-escaped, not embedded raw -- build
        // the wire bytes via the serializer rather than hand-writing a
        // literal control byte into a Rust string (which would produce
        // syntactically INVALID JSON, not exercise the sanitizer).
        let body = serde_json::json!({"key": "a\u{0000}b\u{001b}c"}).to_string();
        let decoded = decode_untrusted_body(body.as_bytes()).unwrap();
        assert_eq!(decoded["key"], "abc");
    }

    #[test]
    fn tab_newline_and_cr_survive_as_legitimate_whitespace() {
        let body = "{\"key\": \"line1\\nline2\\ttabbed\\r\"}".as_bytes();
        let decoded = decode_untrusted_body(body).unwrap();
        assert_eq!(decoded["key"], "line1\nline2\ttabbed\r");
    }

    #[test]
    fn a_rtl_override_is_stripped() {
        // The canonical R14-F3 spoofing example: config<RLO>drowssap
        // would otherwise render as "configpassword" in a UI.
        let body = "{\"key\": \"config\u{202e}drowssap\"}".as_bytes();
        let decoded = decode_untrusted_body(body).unwrap();
        assert_eq!(decoded["key"], "configdrowssap");
    }

    #[test]
    fn a_bom_and_zero_width_space_are_stripped() {
        let body = "{\"key\": \"a\u{feff}b\u{200b}c\"}".as_bytes();
        let decoded = decode_untrusted_body(body).unwrap();
        assert_eq!(decoded["key"], "abc");
    }

    #[test]
    fn combining_marks_are_capped_not_removed() {
        // 6 combining marks stacked on one base char -- capped to 4.
        let mark = '\u{0301}'; // COMBINING ACUTE ACCENT (Mn)
        let value = format!("e{}", mark.to_string().repeat(6));
        let body = serde_json::json!({"key": value}).to_string();
        let decoded = decode_untrusted_body(body.as_bytes()).unwrap();
        let result = decoded["key"].as_str().unwrap();
        assert_eq!(result.chars().count(), 1 + 4);
    }

    #[test]
    fn legitimate_combining_marks_within_the_cap_survive() {
        // Vietnamese-style stacking of 2 marks must NOT be touched.
        let value = "a\u{0301}\u{0300}"; // acute + grave, 2 marks
        let body = serde_json::json!({"key": value}).to_string();
        let decoded = decode_untrusted_body(body.as_bytes()).unwrap();
        assert_eq!(decoded["key"], value);
    }

    #[test]
    fn nested_dict_and_list_leaves_are_all_sanitized() {
        let body = serde_json::json!({"a": {"b": ["x\u{0000}y", "z"]}}).to_string();
        let decoded = decode_untrusted_body(body.as_bytes()).unwrap();
        assert_eq!(decoded["a"]["b"][0], "xy");
        assert_eq!(decoded["a"]["b"][1], "z");
    }

    #[test]
    fn dict_keys_are_left_untouched() {
        // Historical behavior: only values are sanitized, never keys.
        let body = serde_json::json!({"k\u{0000}ey": "v"}).to_string();
        let result = decode_untrusted_body(body.as_bytes());
        // The raw key with the embedded NUL survives JSON parsing
        // (serde_json preserves it) -- confirms keys are untouched,
        // matching Python exactly.
        assert!(result.unwrap().contains_key("k\u{0000}ey"));
    }

    #[test]
    fn invalid_utf8_is_rejected_cleanly() {
        let body: &[u8] = &[0x7B, 0xFF, 0xFE, 0x7D]; // "{", invalid bytes, "}"
        let err = decode_untrusted_body(body).unwrap_err();
        assert_eq!(err.to_string(), "request body is not valid UTF-8");
    }

    #[test]
    fn malformed_json_is_rejected_cleanly_not_recovered() {
        // Deliberate simplification: no aggressive recovery attempt.
        let body = b"{\"a\": }";
        let err = decode_untrusted_body(body).unwrap_err();
        assert_eq!(err.to_string(), "request body is not valid JSON");
    }

    #[test]
    fn a_top_level_array_is_rejected() {
        let body = b"[1, 2, 3]";
        let err = decode_untrusted_body(body).unwrap_err();
        assert_eq!(err.to_string(), "request body must be a JSON object");
    }

    #[test]
    fn a_top_level_scalar_is_rejected() {
        let body = b"\"just a string\"";
        let err = decode_untrusted_body(body).unwrap_err();
        assert_eq!(err.to_string(), "request body must be a JSON object");
    }

    fn nested_body(depth: usize) -> String {
        let mut body = "{\"a\":".repeat(depth);
        body.push('1');
        body.push_str(&"}".repeat(depth));
        body
    }

    #[test]
    fn excessively_deep_nesting_is_rejected_before_parsing() {
        // Exercise a spread of depths, not just one far past
        // MAX_NESTING_DEPTH: 165 sits inside what used to be the F16 gap
        // (127-199, back when MAX_NESTING_DEPTH was 200) where a body
        // sailed past this pre-scan and was rejected by `serde_json`
        // itself with the WRONG message instead of this one. See
        // `depth_just_past_the_serde_json_recursion_limit_gets_the_correct_message`
        // for the exact regression pin.
        for depth in [MAX_NESTING_DEPTH + 1, MAX_NESTING_DEPTH + 10, 165, 250] {
            let err = decode_untrusted_body(nested_body(depth).as_bytes()).unwrap_err();
            assert_eq!(err.to_string(), "request body is too deeply nested");
        }
    }

    #[test]
    fn depth_at_the_serde_json_recursion_limit_is_still_accepted() {
        // MAX_NESTING_DEPTH is pinned to serde_json 1.0.151's own
        // empirically-measured `Value` recursion limit (see
        // `serde_json_recursion_limit_matches_the_boundary_this_module_assumes`).
        // A body exactly at that limit must still decode normally --
        // proves the fix didn't overshoot into rejecting legitimate,
        // moderately-nested request bodies.
        let decoded = decode_untrusted_body(nested_body(MAX_NESTING_DEPTH).as_bytes()).unwrap();
        let mut cursor = &Value::Object(decoded);
        for _ in 0..MAX_NESTING_DEPTH {
            cursor = &cursor["a"];
        }
        assert_eq!(cursor, &Value::from(1));
    }

    #[test]
    fn depth_just_past_the_serde_json_recursion_limit_gets_the_correct_message() {
        // F16: pins the exact bug. Before the fix, `MAX_NESTING_DEPTH`
        // (200) was looser than serde_json 1.0.151's own internal
        // recursion limit for `Value` deserialization (empirically 127 --
        // see the canary test below). A body one level past 127 used to
        // sail past this module's pre-scan (127 < 200), then fail inside
        // `serde_json::from_str` itself, whose error got mapped to the
        // generic "not valid JSON" message instead of "too deeply
        // nested" -- misleading, since the body IS syntactically valid
        // JSON, just deep. This must now be rejected by this module's
        // OWN guard, with the correct message.
        let err = decode_untrusted_body(nested_body(MAX_NESTING_DEPTH + 1).as_bytes()).unwrap_err();
        assert_eq!(err.to_string(), "request body is too deeply nested");
    }

    #[test]
    fn serde_json_recursion_limit_matches_the_boundary_this_module_assumes() {
        // Canary, independent of `exceeds_max_nesting_depth`: calls
        // `serde_json::from_str` directly (bypassing this module's own
        // pre-scan) to reconfirm serde_json's real enforced recursion
        // limit sits exactly where `MAX_NESTING_DEPTH` assumes. If a
        // future serde_json upgrade shifts this limit, this test fails
        // loudly instead of silently reopening the F16 message-mismatch
        // gap (a tightened limit) or silently weakening the actual
        // stack-overflow safety property (a loosened/removed limit).
        assert!(
            serde_json::from_str::<Value>(&nested_body(MAX_NESTING_DEPTH)).is_ok(),
            "serde_json now rejects a body at MAX_NESTING_DEPTH ({}) -- its \
             recursion limit tightened; re-measure and lower MAX_NESTING_DEPTH to match",
            MAX_NESTING_DEPTH
        );
        assert!(
            serde_json::from_str::<Value>(&nested_body(MAX_NESTING_DEPTH + 1)).is_err(),
            "serde_json now accepts a body one level past MAX_NESTING_DEPTH \
             ({}) -- its recursion limit loosened or was removed, so this \
             module's pre-scan is no longer the binding safety limit against \
             a native stack overflow; re-measure and raise MAX_NESTING_DEPTH",
            MAX_NESTING_DEPTH
        );
    }

    #[test]
    fn brackets_inside_string_literals_do_not_count_toward_depth() {
        // A single string leaf packed with brace characters must not
        // trip the depth guard -- it's flat JSON, not nested.
        let value = "{".repeat(MAX_NESTING_DEPTH + 50);
        let body = serde_json::json!({"key": value}).to_string();
        let decoded = decode_untrusted_body(body.as_bytes()).unwrap();
        assert_eq!(
            decoded["key"].as_str().unwrap().len(),
            MAX_NESTING_DEPTH + 50
        );
    }

    #[test]
    fn a_lone_surrogate_escape_is_rejected_by_the_parser_itself() {
        // R15-F1 in Python: \uD800 alone (no low-surrogate partner)
        // parses successfully into a `str` carrying an unpaired
        // surrogate, which then crashes downstream (SQLite TEXT
        // binding) unless explicitly stripped. In Rust, `serde_json`
        // refuses to parse the escape into a `char`/`String` at all
        // (verified empirically: this was a live-failing test before
        // `is_hidden_format_char`'s doc comment was corrected) --
        // there is no value to strip because none is ever constructed.
        // A clean 400 here, not a stripped string, is the CORRECT and
        // stronger outcome -- see `is_hidden_format_char`'s doc.
        let body = br#"{"key": "a\ud800b"}"#;
        let err = decode_untrusted_body(body).unwrap_err();
        assert_eq!(err.to_string(), "request body is not valid JSON");
    }
}
