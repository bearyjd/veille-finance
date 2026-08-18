#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Config parsing and validation: fail fast, name the problem precisely.

use std::io::Write;

use tempfile::NamedTempFile;
use veille::config::Config;

fn write_config(content: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().expect("tempfile");
    f.write_all(content.as_bytes()).expect("write");
    f
}

const VALID: &str = r#"
store_path = "/var/lib/veille/veille.sqlite3"

[[tenants]]
slug = "jd"
display_name = "Self"
[tenants.upstream]
base_url = "http://sure-jd:3000"
api_key_env = "VEILLE_API_KEY_JD"

[[tenants]]
slug = "parents"
display_name = "Parents"
[tenants.upstream]
base_url = "http://sure-parents:3000"
api_key_env = "VEILLE_API_KEY_PARENTS"
"#;

#[test]
fn parses_a_valid_config() {
    let f = write_config(VALID);
    let config = Config::load(f.path()).expect("valid config");
    assert_eq!(config.tenants.len(), 2);
    assert_eq!(config.tenants[0].slug, "jd");
    assert_eq!(
        config.tenants[1].upstream.api_key_env,
        "VEILLE_API_KEY_PARENTS"
    );
}

#[test]
fn rejects_duplicate_slugs() {
    let f = write_config(
        r#"
store_path = "/tmp/v.sqlite3"
[[tenants]]
slug = "jd"
display_name = "One"
[tenants.upstream]
base_url = "http://a:3000"
api_key_env = "K1"
[[tenants]]
slug = "jd"
display_name = "Two"
[tenants.upstream]
base_url = "http://b:3000"
api_key_env = "K2"
"#,
    );
    let err = Config::load(f.path()).expect_err("duplicate slug must fail");
    assert!(
        err.to_string().contains("jd"),
        "error should name the slug: {err}"
    );
}

#[test]
fn rejects_empty_tenant_list_and_bad_slugs() {
    let f = write_config(r#"store_path = "/tmp/v.sqlite3""#);
    assert!(
        Config::load(f.path()).is_err(),
        "no tenants at all must fail"
    );

    let f = write_config(
        r#"
store_path = "/tmp/v.sqlite3"
[[tenants]]
slug = "Bad Slug!"
display_name = "X"
[tenants.upstream]
base_url = "http://a:3000"
api_key_env = "K"
"#,
    );
    let err = Config::load(f.path()).expect_err("bad slug must fail");
    assert!(
        err.to_string().contains("slug"),
        "error should mention slug: {err}"
    );
}

#[test]
fn rejects_inline_secrets_masquerading_as_env_names() {
    // An operator pasting the key itself where the env var name belongs is a
    // security foot-gun; a plausible env name never looks like a long token.
    let f = write_config(
        r#"
store_path = "/tmp/v.sqlite3"
[[tenants]]
slug = "jd"
display_name = "Self"
[tenants.upstream]
base_url = "http://a:3000"
api_key_env = "phase0_rest_key_alpha_0123456789abcdef"
"#,
    );
    let err = Config::load(f.path()).expect_err("token-shaped api_key_env must fail");
    assert!(
        err.to_string().contains("api_key_env"),
        "error should point at the field: {err}"
    );
}

#[test]
fn rejects_duplicate_upstreams_across_tenants() {
    // Two tenants naming the same base_url would silently write one
    // household's data into another household's rows.
    let f = write_config(
        r#"
store_path = "/tmp/v.sqlite3"
[[tenants]]
slug = "jd"
display_name = "Self"
[tenants.upstream]
base_url = "http://same:3000"
api_key_env = "K1"
[[tenants]]
slug = "parents"
display_name = "Parents"
[tenants.upstream]
base_url = "http://same:3000"
api_key_env = "K2"
"#,
    );
    let err = Config::load(f.path()).expect_err("duplicate base_url must fail");
    assert!(
        err.to_string().contains("base_url"),
        "error should name the field: {err}"
    );

    let f = write_config(
        r#"
store_path = "/tmp/v.sqlite3"
[[tenants]]
slug = "jd"
display_name = "Self"
[tenants.upstream]
base_url = "http://a:3000"
api_key_env = "SAME_KEY"
[[tenants]]
slug = "parents"
display_name = "Parents"
[tenants.upstream]
base_url = "http://b:3000"
api_key_env = "SAME_KEY"
"#,
    );
    let err = Config::load(f.path()).expect_err("duplicate api_key_env must fail");
    assert!(
        err.to_string().contains("api_key_env"),
        "error should name the field: {err}"
    );
}

#[test]
fn rejects_non_http_base_urls() {
    let f = write_config(
        r#"
store_path = "/tmp/v.sqlite3"
[[tenants]]
slug = "jd"
display_name = "Self"
[tenants.upstream]
base_url = "ftp://a:3000"
api_key_env = "K"
"#,
    );
    let err = Config::load(f.path()).expect_err("non-http scheme must fail");
    assert!(
        err.to_string().contains("http"),
        "error should mention the allowed schemes: {err}"
    );
}

#[test]
fn rejects_absurd_digest_periods() {
    let f = write_config(
        r#"
store_path = "/tmp/v.sqlite3"
[[tenants]]
slug = "jd"
display_name = "Self"
digest_period_days = 0
[tenants.upstream]
base_url = "http://a:3000"
api_key_env = "K"
"#,
    );
    assert!(
        Config::load(f.path()).is_err(),
        "0-day digest period must fail"
    );

    let f = write_config(
        r#"
store_path = "/tmp/v.sqlite3"
[[tenants]]
slug = "jd"
display_name = "Self"
digest_period_days = 4294967295
[tenants.upstream]
base_url = "http://a:3000"
api_key_env = "K"
"#,
    );
    assert!(
        Config::load(f.path()).is_err(),
        "u32::MAX digest period must fail"
    );
}

const RECIPIENT_TENANT_HEADER: &str = r#"
store_path = "/tmp/v.sqlite3"
[[tenants]]
slug = "parents"
display_name = "Parents"
[tenants.upstream]
base_url = "http://a:3000"
api_key_env = "K"
"#;

#[test]
fn watchers_cannot_exist_without_owners() {
    // §2.2: a watcher-only recipient list is surveillance, not oversight.
    let f = write_config(&format!(
        r#"{RECIPIENT_TENANT_HEADER}
[[tenants.recipients]]
name = "JD"
role = "watcher"
email = "jd@example.com"
"#
    ));
    let err = Config::load(f.path()).expect_err("watcher without owner must fail");
    assert!(
        err.to_string().contains("owner"),
        "error should explain the symmetry requirement: {err}"
    );
}

#[test]
fn recipients_parse_with_roles_and_invalid_ones_fail() {
    let f = write_config(&format!(
        r#"{RECIPIENT_TENANT_HEADER}
[[tenants.recipients]]
name = "Mom"
role = "owner"
email = "mom@example.com"
[[tenants.recipients]]
name = "JD"
role = "watcher"
email = "jd@example.com"
"#
    ));
    let config = Config::load(f.path()).expect("valid recipients");
    assert_eq!(config.tenants[0].recipients.len(), 2);

    let f = write_config(&format!(
        r#"{RECIPIENT_TENANT_HEADER}
[[tenants.recipients]]
name = "X"
role = "admin"
email = "x@example.com"
"#
    ));
    assert!(Config::load(f.path()).is_err(), "unknown role must fail");

    let f = write_config(&format!(
        r#"{RECIPIENT_TENANT_HEADER}
[[tenants.recipients]]
name = "X"
role = "owner"
email = "not-an-email"
"#
    ));
    assert!(Config::load(f.path()).is_err(), "email without @ must fail");
}

#[test]
fn digest_day_is_validated() {
    let f = write_config(&format!(
        "{RECIPIENT_TENANT_HEADER}\ndigest_day = \"sunday\"\n"
    ));
    // digest_day is a tenant field; append inside the tenant table instead.
    let _ = f;
    let f = write_config(
        r#"
store_path = "/tmp/v.sqlite3"
[[tenants]]
slug = "parents"
display_name = "Parents"
digest_day = "someday"
[tenants.upstream]
base_url = "http://a:3000"
api_key_env = "K"
"#,
    );
    assert!(Config::load(f.path()).is_err(), "invalid weekday must fail");
}
