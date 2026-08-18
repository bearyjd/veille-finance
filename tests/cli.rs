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
