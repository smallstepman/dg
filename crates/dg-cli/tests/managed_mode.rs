use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run_dg(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dg"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}

fn readonly(path: &Path) -> bool {
    fs::metadata(path).unwrap().permissions().readonly()
}

#[test]
fn init_managed_mode_locks_schema_documents_but_allows_dg_mutations() {
    let project = tempfile::tempdir().unwrap();

    let initialized = run_dg(project.path(), &["init", "--managed"]);
    assert!(
        initialized.status.success(),
        "dg init --managed failed: {}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    assert!(readonly(&project.path().join("docs")));
    assert!(readonly(&project.path().join("docs/architecture")));
    assert!(readonly(&project.path().join("README.md")));
    assert!(project.path().join(".dg/config.toml").is_file());
    assert!(fs::read_to_string(project.path().join(".dg/config.toml"))
        .unwrap()
        .contains("managed = true"));

    let created = run_dg(project.path(), &["new", "adr", "Managed decision"]);
    assert!(
        created.status.success(),
        "dg new failed in managed mode: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let document = project
        .path()
        .join("docs/architecture/adr-001-managed-decision.md");
    assert!(document.is_file());
    assert!(readonly(&document));
    assert!(readonly(&project.path().join("docs/architecture")));

    let updated = run_dg(project.path(), &["set", "ADR-001", "status=accepted"]);
    assert!(
        updated.status.success(),
        "dg set failed in managed mode: {}",
        String::from_utf8_lossy(&updated.stderr)
    );
    assert!(readonly(&document));
    assert!(fs::read_to_string(&document)
        .unwrap()
        .contains("status: accepted"));

    let deleted = run_dg(project.path(), &["delete", "ADR-001"]);
    assert!(
        deleted.status.success(),
        "dg delete failed in managed mode: {}",
        String::from_utf8_lossy(&deleted.stderr)
    );
    assert!(!document.exists());
    assert!(readonly(&project.path().join("docs/architecture")));

    let disabled = run_dg(project.path(), &["managed", "off"]);
    assert!(
        disabled.status.success(),
        "dg managed off failed: {}",
        String::from_utf8_lossy(&disabled.stderr)
    );
    assert!(!readonly(&project.path().join("docs")));
    assert!(!readonly(&project.path().join("README.md")));

    let enabled = run_dg(project.path(), &["managed", "on"]);
    assert!(
        enabled.status.success(),
        "dg managed on failed: {}",
        String::from_utf8_lossy(&enabled.stderr)
    );
    assert!(readonly(&project.path().join("docs")));
    assert!(readonly(&project.path().join("README.md")));
}
