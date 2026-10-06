//! The config document's schema version (#185, spec
//! `docs/superpowers/specs/2026-10-05-component-versioning-design.md`).
//!
//! `schema_version` is the **minimum schema the document needs**: absent means
//! `1`. This build supports up to [`CONFIG_SCHEMA_VERSION`]; a document asking for
//! more is refused outright (never half-applied, so a rejected revision leaves
//! the previous one live).
//!
//! **Bump rule:** any change that adds a config field, even an optional one,
//! bumps [`CONFIG_SCHEMA_VERSION`] and adds the field's dotted path to
//! [`FIELD_SINCE`] with the new number. The parser then refuses a document that
//! uses the field without declaring a `schema_version` that covers it, so an
//! operator cannot adopt a new field by accident while older proxies still run.
//! `deny_unknown_fields` stays on: an older node rejects a newer field loudly.

use serde_norway::Value;

use crate::ConfigError;

/// The newest config schema this build understands.
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

/// `(dotted path, schema version that introduced it)`. Paths walk mappings by key
/// and pass through sequences transparently (`listeners.foo` matches `foo` in
/// every listener). Empty while the schema is still version 1.
pub const FIELD_SINCE: &[(&str, u32)] = &[];

/// The effective declared version of a document: `raw` is its `schema_version`
/// value, if any. Refuses non-integers, `0`, and anything above
/// [`CONFIG_SCHEMA_VERSION`].
pub(crate) fn check_schema_version(raw: Option<&Value>) -> Result<u32, ConfigError> {
    let Some(raw) = raw else { return Ok(1) };
    let declared = raw
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n >= 1)
        .ok_or_else(|| {
            ConfigError::Invalid(format!(
                "schema_version must be a whole number, 1 or higher (got {})",
                shown(raw)
            ))
        })?;
    if declared > CONFIG_SCHEMA_VERSION {
        return Err(ConfigError::SchemaTooNew {
            found: declared,
            max: CONFIG_SCHEMA_VERSION,
        });
    }
    Ok(declared)
}

fn shown(v: &Value) -> String {
    serde_norway::to_string(v).map_or_else(|_| "?".into(), |s| s.trim().to_owned())
}

/// Refuses a document that uses a field introduced after its declared schema.
pub(crate) fn check_fields_since(
    doc: &Value,
    declared: u32,
    table: &[(&str, u32)],
) -> Result<(), ConfigError> {
    for (path, since) in table {
        if *since > declared && has_path(doc, path) {
            return Err(ConfigError::Invalid(format!(
                "`{path}` needs schema_version {since} or higher (the document declares {declared}); \
                 set `schema_version: {since}` once every proxy that will receive it supports it"
            )));
        }
    }
    Ok(())
}

fn has_path(v: &Value, path: &str) -> bool {
    match v {
        Value::Sequence(items) => items.iter().any(|i| has_path(i, path)),
        Value::Mapping(map) => {
            let (head, rest) = path
                .split_once('.')
                .map_or((path, None), |(h, r)| (h, Some(r)));
            map.get(head).is_some_and(|child| match rest {
                None => true,
                Some(rest) => has_path(child, rest),
            })
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_str;

    const BODY: &str = "pools:\n  - name: mc\n    targets: [\"127.0.0.1:25566\"]\nlisteners:\n  - name: main\n    bind: \"0.0.0.0:25565\"\n    protocol: tcp\n    pool: mc\n";

    fn doc(header: &str) -> String {
        format!("{header}{BODY}")
    }

    #[test]
    fn absent_schema_version_is_current() {
        let cfg = parse_str(BODY).unwrap();
        assert_eq!(cfg.schema_version, 1);
    }

    #[test]
    fn schema_version_1_ok() {
        assert_eq!(
            parse_str(&doc("schema_version: 1\n"))
                .unwrap()
                .schema_version,
            1
        );
    }

    #[test]
    fn schema_version_2_is_rejected_with_message() {
        let e = parse_str(&doc("schema_version: 2\n")).unwrap_err();
        assert!(matches!(e, ConfigError::SchemaTooNew { found: 2, max: 1 }));
        assert_eq!(
            e.to_string(),
            "config schema_version 2 is newer than this build supports (max 1)"
        );
    }

    #[test]
    fn a_newer_document_with_a_field_this_build_lacks_reports_the_version() {
        let e = parse_str(&doc("schema_version: 2\nbrand_new_setting: true\n")).unwrap_err();
        assert!(
            matches!(e, ConfigError::SchemaTooNew { found: 2, max: 1 }),
            "{e}"
        );
    }

    #[test]
    fn an_explicit_null_schema_version_is_rejected_not_read_as_absent() {
        for header in [
            "schema_version: null\n",
            "schema_version: ~\n",
            "schema_version:\n",
        ] {
            let e = parse_str(&doc(header)).unwrap_err().to_string();
            assert!(
                e.contains("schema_version must be a whole number"),
                "{header:?}: {e}"
            );
        }
    }

    #[test]
    fn the_config_records_whether_the_document_declared_a_schema() {
        assert!(!parse_str(BODY).unwrap().schema_declared);
        assert!(
            parse_str(&doc("schema_version: 1\n"))
                .unwrap()
                .schema_declared
        );
    }

    #[test]
    fn schema_version_zero_rejected() {
        let e = parse_str(&doc("schema_version: 0\n"))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("schema_version must be a whole number, 1 or higher (got 0)"),
            "{e}"
        );
    }

    #[test]
    fn string_schema_version_rejected() {
        let e = parse_str(&doc("schema_version: \"2\"\n"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("schema_version must be a whole number"), "{e}");
        assert!(!e.contains("invalid type"), "no serde dump: {e}");
    }

    #[test]
    fn negative_and_huge_schema_versions_are_rejected() {
        for v in ["-1", "99999999999", "1.5"] {
            let e = parse_str(&doc(&format!("schema_version: {v}\n"))).unwrap_err();
            assert!(e.to_string().contains("schema_version must be"), "{v}: {e}");
        }
    }

    #[test]
    fn document_using_a_field_newer_than_its_declared_schema_is_rejected() {
        let value: Value = serde_norway::from_str(
            "settings:\n  fancy: true\nlisteners:\n  - name: a\n    shiny: 1\n",
        )
        .unwrap();
        let table = [("settings.fancy", 2), ("listeners.shiny", 3)];
        let e = check_fields_since(&value, 1, &table)
            .unwrap_err()
            .to_string();
        assert!(e.contains("`settings.fancy` needs schema_version 2"), "{e}");
        // Declaring 2 covers the first field but not the one behind a sequence.
        let e = check_fields_since(&value, 2, &table)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("`listeners.shiny` needs schema_version 3"),
            "{e}"
        );
        assert!(check_fields_since(&value, 3, &table).is_ok());
        // A field the document does not use never triggers.
        assert!(check_fields_since(&value, 1, &[("settings.other", 9)]).is_ok());
    }

    #[test]
    fn field_since_table_covers_every_raw_field_added_after_v1() {
        // Every path must name a real field, so the table cannot rot: put the
        // path into a document and require that the schema does not call it
        // unknown. (Empty while the schema is at version 1.)
        for (path, since) in FIELD_SINCE {
            assert!(*since >= 2, "{path}: version 1 needs no entry");
            assert!(
                *since <= CONFIG_SCHEMA_VERSION,
                "{path}: bump CONFIG_SCHEMA_VERSION"
            );
            let mut v = Value::Null;
            for seg in path.rsplit('.') {
                let mut m = serde_norway::Mapping::new();
                m.insert(Value::String(seg.into()), v);
                v = Value::Mapping(m);
            }
            let text = serde_norway::to_string(&v).unwrap();
            let e = parse_str(&format!("schema_version: {since}\n{text}"))
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(!e.contains("unknown field"), "{path}: {e}");
        }
    }
}
