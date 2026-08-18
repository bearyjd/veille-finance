#![allow(clippy::expect_used, clippy::unwrap_used)]

//! CLI-level behavior that the library API cannot capture: `--dry-run` must
//! be inert at the PROCESS level, not just inside the engine.

use std::io::Write;
use std::process::Command;

use tempfile::TempDir;

fn write_config(dir: &TempDir, store_path: &std::path::Path) -> std::path::PathBuf {
    let path = dir.path().join("veille.toml");
    let mut f = std::fs::File::create(&path).expect("config file");
    writeln!(
        f,
        r#"store_path = "{}"

[[tenants]]
slug = "jd"
display_name = "Self"
[tenants.upstream]
base_url = "http://localhost:9"
api_key_env = "VEILLE_API_KEY_JD"
"#,
        store_path.display()
    )
    .expect("write config");
    path
}

#[test]
fn dry_run_evaluate_does_not_create_a_store() {
    let dir = TempDir::new().expect("tempdir");
    let store_path = dir.path().join("does-not-exist.sqlite3");
    let config = write_config(&dir, &store_path);

    let output = Command::new(env!("CARGO_BIN_EXE_veille"))
        .args([
            "evaluate",
            "--dry-run",
            "--as-of",
            "2026-08-18T22:00:00Z",
            "--config",
        ])
        .arg(&config)
        .output()
        .expect("run veille");

    assert!(
        !output.status.success(),
        "dry-run against a store that was never synced must fail loudly"
    );
    assert!(
        !store_path.exists(),
        "dry-run must not create the store file"
    );
}

#[test]
fn dry_run_evaluate_does_not_insert_tenant_rows() {
    let dir = TempDir::new().expect("tempdir");
    let store_path = dir.path().join("store.sqlite3");
    let config = write_config(&dir, &store_path);

    // Create the store with a DIFFERENT tenant only.
    let rt = tokio::runtime::Runtime::new().expect("rt");
    rt.block_on(async {
        let store = veille::store::Store::open(&store_path)
            .await
            .expect("store");
        store
            .tenants()
            .ensure("someone-else", "Other")
            .await
            .expect("tenant");
    });

    let output = Command::new(env!("CARGO_BIN_EXE_veille"))
        .args([
            "evaluate",
            "--dry-run",
            "--as-of",
            "2026-08-18T22:00:00Z",
            "--config",
        ])
        .arg(&config)
        .output()
        .expect("run veille");
    assert!(
        !output.status.success(),
        "the configured tenant has no data; dry-run must report that, not create it"
    );

    rt.block_on(async {
        let store = veille::store::Store::open(&store_path)
            .await
            .expect("store");
        assert!(
            store.tenants().by_slug("jd").await.is_err(),
            "dry-run must not insert tenant rows"
        );
    });
}

fn synced_evaluated_store(dir: &TempDir) -> std::path::PathBuf {
    let store_path = dir.path().join("digest-store.sqlite3");
    let rt = tokio::runtime::Runtime::new().expect("rt");
    rt.block_on(async {
        use chrono::TimeZone;
        let store = veille::store::Store::open(&store_path)
            .await
            .expect("store");
        let tenant = store.tenants().ensure("jd", "Self").await.expect("tenant");
        let source = veille::source::fixture::FixtureSureSource::new(format!(
            "{}/fixtures/synthetic",
            env!("CARGO_MANIFEST_DIR")
        ));
        let sync_now = chrono::Utc
            .with_ymd_and_hms(2026, 8, 17, 22, 0, 0)
            .single()
            .expect("ts");
        veille::sync::sync_tenant(&store, tenant, &source, sync_now, Default::default())
            .await
            .expect("sync");
        let as_of = chrono::Utc
            .with_ymd_and_hms(2026, 8, 20, 22, 0, 0)
            .single()
            .expect("ts");
        veille::rules::evaluate_tenant(
            &store,
            tenant,
            veille::config::RuleThresholds::default(),
            as_of,
            true,
        )
        .await
        .expect("evaluate");
    });
    store_path
}

#[test]
fn digest_renders_with_no_llm_configured() {
    let dir = TempDir::new().expect("tempdir");
    let store_path = synced_evaluated_store(&dir);
    let config = write_config(&dir, &store_path);

    let output = Command::new(env!("CARGO_BIN_EXE_veille"))
        .args(["digest", "--as-of", "2026-08-20T22:00:00Z", "--config"])
        .arg(&config)
        .env_remove("VEILLE_LLM_BASE_URL")
        .env_remove("VEILLE_LLM_API_KEY")
        .env_remove("VEILLE_LLM_MODEL")
        .output()
        .expect("run veille");

    assert!(
        output.status.success(),
        "digest must render without any LLM: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("First National"),
        "findings present: {stdout}"
    );
    assert!(
        stdout.contains("Everyday Checking"),
        "accounts present: {stdout}"
    );
}

#[test]
fn digest_renders_when_llm_endpoint_is_unreachable() {
    let dir = TempDir::new().expect("tempdir");
    let store_path = synced_evaluated_store(&dir);
    let config = write_config(&dir, &store_path);

    let output = Command::new(env!("CARGO_BIN_EXE_veille"))
        .args(["digest", "--as-of", "2026-08-20T22:00:00Z", "--config"])
        .arg(&config)
        .env("VEILLE_LLM_BASE_URL", "http://127.0.0.1:1/v1")
        .env("VEILLE_LLM_API_KEY", "dead-key")
        .env("VEILLE_LLM_MODEL", "cheap-think")
        .output()
        .expect("run veille");

    assert!(
        output.status.success(),
        "an unreachable LLM must never block the digest: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("First National"),
        "findings still present: {stdout}"
    );
}
