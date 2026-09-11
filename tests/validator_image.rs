//! Exercises the offline image entrypoint with disposable source trees and real Make recipes.

use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(recipe: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "ai-sre-image-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("source")).unwrap();
        fs::create_dir(root.join("scratch")).unwrap();
        fs::write(root.join("source/Makefile"), recipe).unwrap();
        Self(root)
    }

    fn run(&self) -> std::process::Output {
        Command::new("/bin/sh")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/packaging/validator/validate-u9"
            ))
            .arg(self.0.join("source"))
            .arg(self.0.join("scratch"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            // An inherited dry-run flag must not turn real checks into printed commands.
            .env("MAKEFLAGS", "n")
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn checks_run_in_disposable_scratch_without_changing_source() {
    // Given a check that generates a file as homelab inventory rendering does.
    let fixture = Fixture::new("ci:\n\t@echo checked > generated\n");
    // When the image entrypoint runs the real Make recipe.
    let output = fixture.run();
    // Then generated output belongs only to scratch, never the captured source.
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.0.join("source/generated").exists());
    assert_eq!(
        fs::read_to_string(fixture.0.join("scratch/repository/generated")).unwrap(),
        "checked\n"
    );
}

#[test]
fn a_failed_check_is_not_reported_as_validation_success() {
    // Given a substantive check that rejects the repository.
    let fixture = Fixture::new("ci:\n\t@exit 7\n");
    // When the entrypoint invokes that check.
    let output = fixture.run();
    // Then its failure reaches the container caller instead of a success footer masking it.
    assert!(!output.status.success());
}

#[test]
fn an_existing_scratch_tree_is_not_reused() {
    // Given a previous validation's scratch tree.
    let fixture = Fixture::new("ci:\n\t@echo checked > generated\n");
    assert!(fixture.run().status.success());
    // When another invocation attempts to reuse it.
    let output = fixture.run();
    // Then stale output cannot stand in for a fresh run.
    assert!(!output.status.success());
}
