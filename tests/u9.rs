//! Protects the shadow GitOps renderer's one-field boundary without granting write authority.

use ai_sre::gitops::it_tools_image::render;

#[tokio::test]
async fn validator_refuses_an_unverified_runtime() {
    use ai_sre::gitops::sandbox::RootlessValidator;
    // Given a path and digest that cannot identify a protected local Podman installation.
    let path = std::path::Path::new("/nonexistent/ai-sre-podman");
    // When runtime admission is requested, before any repository workload can run.
    let result = RootlessValidator::connect(path, &format!("sha256:{}", "a".repeat(64))).await;
    // Then no usable validator is returned and there is no rootful fallback.
    assert!(result.is_err());
}

#[tokio::test]
async fn snapshot_uses_committed_files_not_dirty_worktree_or_git_credentials() {
    use ai_sre::gitops::snapshot::RepositorySnapshot;
    // Given a disposable repository with a committed pilot and a dirty local replacement.
    let root = std::env::temp_dir().join(format!("ai-sre-u9-source-{}", std::process::id()));
    std::fs::create_dir_all(root.join("stacks/utility")).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("/usr/bin/git")
            .args(args)
            .env_clear()
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success());
        output.stdout
    };
    git(&["init", "-b", "main"]);
    let path = root.join("stacks/utility/compose.yml");
    let source = "services:\n  it-tools:\n    image: ghcr.io/corentinth/it-tools:latest\n";
    std::fs::write(&path, source).unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-m",
        "fixture",
    ]);
    let base = String::from_utf8(git(&["rev-parse", "HEAD"])).unwrap();
    std::fs::write(&path, "local secret must not enter the sandbox").unwrap();
    let image = format!("ghcr.io/corentinth/it-tools@sha256:{}", "a".repeat(64));
    // When an immutable snapshot is prepared for the exact committed base.
    let snapshot = RepositorySnapshot::capture(&root, base.trim(), &image)
        .await
        .unwrap();
    // Then only Git object contents and the deterministic pilot replacement are captured.
    assert_eq!(snapshot.original_source(), source);
    assert_eq!(snapshot.base_sha(), base.trim());
    assert_eq!(snapshot.pilot_source(), render(source, &image).unwrap());
    assert!(snapshot.digest().starts_with("sha256:"));
    drop(snapshot);

    // Given a local replacement ref that substitutes different contents for the committed blob.
    let blob = String::from_utf8(git(&["rev-parse", "HEAD:stacks/utility/compose.yml"])).unwrap();
    let replacement =
        String::from_utf8(git(&["hash-object", "-w", "stacks/utility/compose.yml"])).unwrap();
    git(&["replace", blob.trim(), replacement.trim()]);
    // When the same commit is captured again despite local Git replacement metadata.
    let snapshot = RepositorySnapshot::capture(&root, base.trim(), &image)
        .await
        .unwrap();
    // Then the original immutable blob, not the replacement ref, supplies the validation source.
    assert_eq!(snapshot.original_source(), source);
    drop(snapshot);

    git(&["replace", "-d", blob.trim()]);
    // Given a partial clone whose missing pilot blob would normally invoke a host helper.
    let object = root
        .join(".git/objects")
        .join(&blob.trim()[..2])
        .join(&blob.trim()[2..]);
    let saved = std::fs::read(&object).unwrap();
    std::fs::remove_file(&object).unwrap();
    let marker = root.join("helper-was-invoked");
    git(&[
        "config",
        "remote.origin.url",
        "ssh://example.invalid/review",
    ]);
    git(&["config", "remote.origin.promisor", "true"]);
    git(&[
        "config",
        "core.sshCommand",
        &format!("/usr/bin/touch '{}'; /usr/bin/false", marker.display()),
    ]);
    // When snapshot capture encounters an object that is not available locally.
    let missing = RepositorySnapshot::capture(&root, base.trim(), &image).await;
    // Then capture fails without invoking any transport or repository-configured helper.
    assert!(missing.is_err());
    assert!(!marker.exists(), "snapshot invoked a host helper");
    std::fs::write(object, saved).unwrap();

    // Given a new commit containing a link to a host path outside the repository.
    std::os::unix::fs::symlink("/etc/passwd", root.join("host-file")).unwrap();
    git(&["add", "host-file"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-m",
        "symlink",
    ]);
    let linked_base = String::from_utf8(git(&["rev-parse", "HEAD"])).unwrap();
    // When either stale evidence or a committed symlink is offered for capture.
    let stale = RepositorySnapshot::capture(&root, base.trim(), &image).await;
    let linked = RepositorySnapshot::capture(&root, linked_base.trim(), &image).await;
    // Then neither can produce a sandbox input tree.
    assert!(stale.is_err());
    assert!(linked.is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn sandbox_scratch_budget_covers_all_writable_scratch_mounts() {
    use ai_sre::gitops::sandbox::{SandboxLimits, SandboxPlan};
    // Given a 64 MiB total allowance, including temporary files and shared memory.
    let plan = SandboxPlan::new(
        &format!(
            "ghcr.io/bodhispace-xyz/ai-sre-validator@sha256:{}",
            "a".repeat(64)
        ),
        SandboxLimits {
            scratch_mib: 64,
            ..SandboxLimits::default()
        },
    )
    .unwrap();
    // When the runtime's independent scratch mounts are configured.
    let argv = plan.arguments("u9-budget", "/tmp/source").unwrap();
    // Then work, temporary files, and shared memory sum to 64 MiB, not separate extra allowances.
    assert!(
        argv.iter()
            .any(|arg| arg == "/work:rw,nosuid,nodev,exec,size=32m,mode=1777")
    );
    assert!(
        argv.iter()
            .any(|arg| arg == "/tmp:rw,nosuid,nodev,noexec,size=16m,mode=1777")
    );
    assert!(argv.iter().any(|arg| arg == "--shm-size=16m"));
    assert!(
        argv.iter()
            .any(|arg| arg == "/dev/shm:rw,nosuid,nodev,noexec,size=16m,mode=1777")
    );
}

#[test]
fn sandbox_plan_has_fixed_isolation_and_rejects_mutable_images() {
    use ai_sre::gitops::sandbox::{SandboxLimits, SandboxPlan};
    // Given a deployment-owned image digest and bounded validation limits.
    let image = format!(
        "ghcr.io/bodhispace-xyz/ai-sre-validator@sha256:{}",
        "a".repeat(64)
    );
    let plan = SandboxPlan::new(&image, SandboxLimits::default()).unwrap();
    // When the fixed sandbox invocation is inspected before any process is started.
    let argv = plan.arguments("u9-test-123", "/tmp/u9-snapshot").unwrap();
    // Then no network, privileges, image pulling, or writable source mount is granted.
    for flag in [
        "--network=none",
        "--read-only",
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
        "--pull=never",
        "--pids-limit=128",
        "--user=65532:65532",
    ] {
        assert!(argv.iter().any(|arg| arg == flag), "missing {flag}");
    }
    assert!(
        argv.iter()
            .any(|arg| arg == "type=bind,src=/tmp/u9-snapshot,dst=/source,ro=true")
    );
    assert_eq!(argv.last().unwrap(), &image);
    assert!(
        SandboxPlan::new(
            "ghcr.io/bodhispace-xyz/ai-sre-validator:latest",
            SandboxLimits::default()
        )
        .is_err()
    );
    assert!(plan.arguments("--privileged", "/tmp/u9-snapshot").is_err());
    assert!(
        plan.arguments("u9-test", "/tmp/source,dst=/secrets")
            .is_err()
    );
}

#[test]
fn service_owned_json_cannot_become_a_protected_deployment_receipt() {
    use ai_sre::gitops::receipt::ProtectedReceipt;
    // Given a JSON file writable by the current service identity, not a protected producer.
    let directory =
        std::env::temp_dir().join(format!("ai-sre-u9-untrusted-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("receipt.json");
    std::fs::write(&path, b"{}").unwrap();
    // When that path is offered to the protected inbox reader.
    let result = ProtectedReceipt::read(&path);
    // Then even syntactically valid JSON cannot cross the provenance boundary.
    assert!(result.is_err());
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn invalid_policy_and_unordered_health_evidence_fail_closed() {
    use ai_sre::gitops::qualified_deployment::{HealthSample, ObservationPolicy, assess_health};
    // Given valid paired observations and configurable bounds.
    let policy = ObservationPolicy {
        window_seconds: 120,
        max_gap_seconds: 60,
        max_age_seconds: 600,
    };
    let samples: Vec<_> = [101, 161, 221]
        .into_iter()
        .map(|at| HealthSample {
            observed_at: at,
            gatus_healthy: true,
            prometheus_healthy: true,
        })
        .collect();
    // When policy arithmetic overflows or observations cannot establish a strict chronology.
    let invalid = ObservationPolicy {
        max_gap_seconds: 0,
        ..policy.clone()
    };
    // Then none of those inputs can qualify health, including empty and duplicated data.
    assert!(assess_health(&invalid, 100, 221, &samples).is_err());
    assert!(assess_health(&policy, u64::MAX - 1, u64::MAX, &samples).is_err());
    assert!(assess_health(&policy, 100, 221, &[]).is_err());
    assert!(
        assess_health(
            &policy,
            100,
            221,
            &[samples[0].clone(), samples[0].clone(), samples[2].clone()]
        )
        .is_err()
    );
    let mut reverse = samples.clone();
    reverse.reverse();
    assert!(assess_health(&policy, 100, 221, &reverse).is_err());
}

#[test]
fn preflight_rejects_moving_base_and_every_non_renderer_change() {
    use ai_sre::gitops::validate::validate_change;
    // Given a repair derived from one exact base revision and fixed pilot path.
    let base = "a".repeat(40);
    let source = "services:\n  it-tools:\n    image: old\n    restart: always\n";
    let image = format!("ghcr.io/corentinth/it-tools@sha256:{}", "b".repeat(64));
    let candidate = render(source, &image).unwrap();
    let paths = ["stacks/utility/compose.yml"];
    // When the same candidate is assessed with current read-only repository evidence.
    assert!(validate_change(&base, &base, source, &candidate, &image, &paths).is_ok());
    // Then base movement, extra files, or a second scalar change invalidates the artifact.
    assert!(validate_change(&base, &"c".repeat(40), source, &candidate, &image, &paths).is_err());
    assert!(
        validate_change(
            &base,
            &base,
            source,
            &candidate.replace("always", "never"),
            &image,
            &paths
        )
        .is_err()
    );
    assert!(
        validate_change(
            &base,
            &base,
            source,
            &candidate,
            &image,
            &[paths[0], ".github/workflows/deploy.yml"]
        )
        .is_err()
    );
}

#[test]
fn health_window_requires_both_sources_after_deployment_without_gaps() {
    use ai_sre::gitops::qualified_deployment::{HealthSample, ObservationPolicy, assess_health};
    // Given a predeclared three-minute window and paired minute-by-minute observations.
    let policy = ObservationPolicy {
        window_seconds: 180,
        max_gap_seconds: 60,
        max_age_seconds: 600,
    };
    let samples: Vec<_> = [101, 160, 220, 280]
        .into_iter()
        .map(|at| HealthSample {
            observed_at: at,
            gatus_healthy: true,
            prometheus_healthy: true,
        })
        .collect();
    // When a completed deployment is assessed against the full window.
    assert!(assess_health(&policy, 100, 280, &samples).is_ok());
    // Then a failed source, early sample, missing coverage, or stale assessment cannot pass.
    let mut unhealthy = samples.clone();
    unhealthy[1].prometheus_healthy = false;
    assert!(assess_health(&policy, 100, 280, &unhealthy).is_err());
    assert!(assess_health(&policy, 101, 281, &samples).is_err());
    assert!(
        assess_health(
            &policy,
            100,
            280,
            &[samples[0].clone(), samples[2].clone(), samples[3].clone()]
        )
        .is_err()
    );
    assert!(assess_health(&policy, 100, 881, &samples).is_err());
}

#[test]
fn health_window_allows_bounded_scrape_jitter_at_the_end() {
    use ai_sre::gitops::qualified_deployment::{HealthSample, ObservationPolicy, assess_health};
    // Given healthy checks arriving just after the required window boundary.
    let policy = ObservationPolicy {
        window_seconds: 120,
        max_gap_seconds: 60,
        max_age_seconds: 600,
    };
    let samples: Vec<_> = [101, 161, 221]
        .into_iter()
        .map(|at| HealthSample {
            observed_at: at,
            gatus_healthy: true,
            prometheus_healthy: true,
        })
        .collect();
    // When the final check covers the full interval without exceeding the permitted gap.
    let result = assess_health(&policy, 100, 221, &samples);
    // Then harmless scrape jitter does not require an impossible exact timestamp match.
    assert!(result.is_ok());
    assert!(assess_health(&policy, 100, 220, &samples).is_err());
}

#[test]
fn image_repair_preserves_every_other_byte() {
    // Given a Compose service surrounded by comments and an unrelated service.
    let before = "services:\n  it-tools:\n    image: ghcr.io/corentinth/it-tools:latest # pinned by repair\n    restart: unless-stopped\n  other:\n    image: example:1\n";
    let image = format!("ghcr.io/corentinth/it-tools@sha256:{}", "a".repeat(64));
    // When the deterministic renderer replaces the pilot image scalar.
    let after = render(before, &image).unwrap();
    // Then only that scalar changes; comments and neighboring services are untouched.
    assert_eq!(
        after,
        before.replacen("ghcr.io/corentinth/it-tools:latest", &image, 1)
    );
}

#[test]
fn ambiguous_or_non_pilot_repairs_produce_no_patch() {
    // Given source layouts that cannot establish exactly one explicit pilot scalar.
    let image = format!("ghcr.io/corentinth/it-tools@sha256:{}", "b".repeat(64));
    for source in [
        "services:\n  other:\n    image: old\n",
        "services:\n  it-tools:\n    image: old\n    image: other\n",
        "services: {it-tools: {image: old}}\n",
        "services:\n  it-tools:\n    image: [old]\n",
        "services:\n  it-tools: &pilot\n    image: old\n  other: *pilot\n",
    ] {
        // When a repair encounters missing, duplicate, indirect, or unsupported YAML.
        let result = render(source, &image);
        // Then it fails closed rather than selecting an arbitrary matching image.
        assert!(result.is_err(), "accepted {source}");
    }
    for image in [
        "ghcr.io/corentinth/it-tools:latest",
        "$(touch /tmp/unsafe)",
        "other@sha256:abc",
    ] {
        assert!(render("services:\n  it-tools:\n    image: old\n", image).is_err());
    }
}

#[test]
fn supported_layouts_change_only_the_pilot_scalar() {
    // Given combinations of indentation, quoting, comments, and line endings.
    let image = format!("ghcr.io/corentinth/it-tools@sha256:{}", "c".repeat(64));
    for indent in [2, 4, 6] {
        for quote in ["", "'", "\""] {
            for newline in ["\n", "\r\n"] {
                let a = " ".repeat(indent);
                let b = " ".repeat(indent * 2);
                let scalar = format!("{quote}old{quote}");
                let before = format!(
                    "services:{newline}{a}it-tools:{newline}{b}image: {scalar} # context{newline}{b}restart: always{newline}"
                );
                // When the supported explicit scalar is replaced.
                let after = render(&before, &image).unwrap();
                // Then every byte outside that scalar remains stable.
                assert_eq!(after, before.replacen(&scalar, &image, 1));
            }
        }
    }
}

#[test]
fn excessive_candidate_count_is_rejected_before_repeated_parsing() {
    // Given an oversized service catalog even though it contains a valid pilot.
    let mut source = "services:\n  it-tools:\n    image: old\n".to_owned();
    for index in 0..64 {
        source.push_str(&format!("  service-{index}:\n    image: example:1\n"));
    }
    let image = format!("ghcr.io/corentinth/it-tools@sha256:{}", "d".repeat(64));
    // When rendering would require excessive full-document candidate checks.
    let result = render(&source, &image);
    // Then the parser work is bounded by rejecting the source rather than continuing.
    assert!(result.is_err());
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires the root-provisioned synthetic Linux receipt fixtures"]
fn linux_receipt_inbox_enforces_ownership_modes_and_links() {
    use ai_sre::gitops::receipt::ProtectedReceipt;
    use std::os::unix::fs::MetadataExt;
    // Given synthetic receipts provisioned by root outside writable ancestors.
    // These files test filesystem provenance, not the truth of a homelab deployment.
    let root = std::path::PathBuf::from(
        std::env::var("U9_RECEIPT_FIXTURES").expect("set protected fixture directory"),
    );
    assert_ne!(
        std::fs::metadata("/proc/self").unwrap().uid(),
        0,
        "run this acceptance test as the non-root service user"
    );

    // When the same receipt is read through protected, writable, and linked paths.
    let receipt = ProtectedReceipt::read(&root.join("protected.json")).unwrap();
    assert_eq!(receipt.deployment_id(), "synthetic-linux-acceptance");
    for rejected in [
        "writable.json",
        "user-owned.json",
        "symlink.json",
        "hardlink.json",
        "writable-parent/receipt.json",
    ] {
        // Then only a root-owned regular file under protected directories is trusted.
        assert!(
            ProtectedReceipt::read(&root.join(rejected)).is_err(),
            "accepted {rejected}"
        );
    }
}
