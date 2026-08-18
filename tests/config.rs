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
