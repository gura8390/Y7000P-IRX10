use super::*;
use std::{
    fs,
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(std::env::temp_dir().join(format!(
            "JiksStartupArguments-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
    fn write(&self, bytes: &[u8]) {
        fs::create_dir_all(&self.0).unwrap();
        fs::write(self.0.join(FILE_NAME), bytes).unwrap();
    }
    fn preferences(&self, minimized: bool, read_only: bool) -> Preferences {
        Preferences {
            minimized,
            read_only,
            expected_revision: load(&self.0).unwrap().revision,
        }
    }
    fn pending(&self) -> usize {
        fs::read_dir(&self.0)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".pending"))
            .count()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn missing_load_and_noop_save_do_not_create_default_files_or_directories() {
    let fixture = Fixture::new();
    let loaded = load(&fixture.0).unwrap();
    assert!(!loaded.minimized && !loaded.read_only);
    assert_eq!(loaded.inert_legacy_count, 0);
    assert_eq!(loaded.revision, "missing");
    assert!(!fixture.0.exists());
    let result = save(&fixture.0, &fixture.preferences(false, false)).unwrap();
    assert!(!result.file_exists && !fixture.0.exists());
}

#[test]
fn only_exact_supported_whole_lines_are_consumed_internal_cli_and_quotes_are_inert() {
    let fixture = Fixture::new();
    fixture.write(b"\xef\xbb\xbf--minimized\r\n--read-only\r--data-dir=D:\\synthetic\n--quit-after-ms=1\r\nbroker\ndoctor\nui-smoke\nui-layout\nui-benchmark\n--benchmark-log=C:\\synthetic\n\"--minimized\"\n --read-only\n--MINIMIZED\n--minimized=true\n--read-only=false\n");
    let loaded = load(&fixture.0).unwrap();
    assert!(loaded.minimized && loaded.read_only);
    assert_eq!(loaded.inert_legacy_count, 13);
    let snapshot = loaded.snapshot();
    assert!(snapshot.legacy_contents_redacted);
    let serialized = serde_json::to_string(&snapshot).unwrap();
    assert!(!serialized.contains("synthetic") && !serialized.contains("broker"));
    assert!(!format!("{loaded:?}").contains("synthetic"));
}

#[test]
fn save_preserves_bom_unicode_unknown_secrets_quotes_order_and_all_newline_styles() {
    let fixture = Fixture::new();
    let untouched = "--proxy-password=\"synthetic secret\\\\\"\r\n--proxy-url=https://invalid.example/a?token=fixture\n未知参数=设置\r\"--minimized\"\n--data-dir=D:\\synthetic path";
    let mut original = BOM.to_vec();
    original.extend_from_slice(format!("--minimized\r\n{untouched}").as_bytes());
    fixture.write(&original);
    let result = save(&fixture.0, &fixture.preferences(false, true)).unwrap();
    assert!(!result.minimized && result.read_only);
    let expected = [BOM, format!("{untouched}\r\n--read-only\r\n").as_bytes()].concat();
    assert_eq!(fs::read(fixture.0.join(FILE_NAME)).unwrap(), expected);
    assert_eq!(
        fs::read(fixture.0.join("args.txt.previous")).unwrap(),
        original
    );
    assert_eq!(fixture.pending(), 0);
    assert!(!format!("{:?}", load(&fixture.0).unwrap()).contains("secret"));
}

#[test]
fn duplicates_are_removed_only_when_disabled_and_noop_does_not_touch_previous() {
    let fixture = Fixture::new();
    let original = b"--minimized\r--minimized\nopaque=one\r\n--read-only";
    fixture.write(original);
    fs::write(fixture.0.join("args.txt.previous"), b"earlier").unwrap();
    save(&fixture.0, &fixture.preferences(true, true)).unwrap();
    assert_eq!(fs::read(fixture.0.join(FILE_NAME)).unwrap(), original);
    assert_eq!(
        fs::read(fixture.0.join("args.txt.previous")).unwrap(),
        b"earlier"
    );
    save(&fixture.0, &fixture.preferences(false, false)).unwrap();
    assert_eq!(
        fs::read(fixture.0.join(FILE_NAME)).unwrap(),
        b"opaque=one\r\n"
    );
}

#[test]
fn malformed_encoding_nul_and_oversize_fail_without_writing_or_leaking_contents() {
    for original in [
        vec![0xff, 0xfe, b's', 0],
        b"secret=fixture\0\n--minimized".to_vec(),
        vec![b'x'; MAX_FILE_BYTES + 1],
    ] {
        let fixture = Fixture::new();
        fixture.write(&original);
        assert!(load(&fixture.0).is_err());
        let result = save(
            &fixture.0,
            &Preferences {
                minimized: true,
                read_only: false,
                expected_revision: revision(Some(&original)),
            },
        );
        assert!(result.is_err());
        assert!(!format!("{:#}", result.unwrap_err()).contains("secret=fixture"));
        assert_eq!(fs::read(fixture.0.join(FILE_NAME)).unwrap(), original);
        assert!(!fixture.0.join("args.txt.previous").exists());
        assert_eq!(fixture.pending(), 0);
    }
}

#[test]
fn stale_revision_and_precommit_external_change_are_never_overwritten() {
    let fixture = Fixture::new();
    fixture.write(b"first\n");
    let preferences = fixture.preferences(true, false);
    fixture.write(b"external\n");
    assert!(save(&fixture.0, &preferences).is_err());
    assert_eq!(fs::read(fixture.0.join(FILE_NAME)).unwrap(), b"external\n");
    let preferences = fixture.preferences(true, false);
    let result = save_with_checkpoint(&fixture.0, &preferences, |stage| {
        if stage == CommitStage::BeforeCommit {
            fs::write(fixture.0.join(FILE_NAME), b"external newer\n")?;
        }
        Ok(())
    });
    assert!(result.is_err());
    assert_eq!(
        fs::read(fixture.0.join(FILE_NAME)).unwrap(),
        b"external newer\n"
    );
    assert_eq!(fixture.pending(), 0);
}

#[test]
fn precommit_injected_failures_preserve_old_complete_bytes_and_cleanup_pending() {
    for failed in [
        CommitStage::BeforeTemporary,
        CommitStage::AfterTemporary,
        CommitStage::BeforeCommit,
    ] {
        let fixture = Fixture::new();
        fixture.write(b"opaque=future\r\n");
        let preferences = fixture.preferences(true, true);
        let result = save_with_checkpoint(&fixture.0, &preferences, |stage| {
            if stage == failed {
                Err(io::Error::other("synthetic checkpoint failure"))
            } else {
                Ok(())
            }
        });
        assert!(result.is_err());
        assert_eq!(
            fs::read(fixture.0.join(FILE_NAME)).unwrap(),
            b"opaque=future\r\n"
        );
        assert_eq!(fixture.pending(), 0);
    }
}

#[test]
fn actual_sharing_failure_preserves_original_and_retry_commits_with_previous() {
    let fixture = Fixture::new();
    fixture.write(b"opaque=fixture\n");
    let preferences = fixture.preferences(true, false);
    let held = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(fixture.0.join(FILE_NAME))
        .unwrap();
    assert!(save(&fixture.0, &preferences).is_err());
    assert_eq!(
        fs::read(fixture.0.join(FILE_NAME)).unwrap(),
        b"opaque=fixture\n"
    );
    assert_eq!(fixture.pending(), 0);
    drop(held);
    assert!(save(&fixture.0, &preferences).unwrap().minimized);
    assert_eq!(
        fs::read(fixture.0.join("args.txt.previous")).unwrap(),
        b"opaque=fixture\n"
    );
    assert_eq!(fixture.pending(), 0);
}

#[test]
fn concurrent_editors_with_one_revision_cannot_silently_overwrite_each_other() {
    let fixture = Fixture::new();
    fixture.write(b"--proxy-password=fixture\n");
    let first = fixture.preferences(true, false);
    let second = fixture.preferences(false, true);
    let barrier = Arc::new(Barrier::new(3));
    let root = fixture.0.clone();
    let a = barrier.clone();
    let one = std::thread::spawn(move || {
        a.wait();
        save(&root, &first)
    });
    let root = fixture.0.clone();
    let a = barrier.clone();
    let two = std::thread::spawn(move || {
        a.wait();
        save(&root, &second)
    });
    barrier.wait();
    let results = [one.join().unwrap(), two.join().unwrap()];
    assert_eq!(results.iter().filter(|v| v.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|v| v.is_err()).count(), 1);
    let loaded = load(&fixture.0).unwrap();
    assert_ne!(loaded.minimized, loaded.read_only);
    assert_eq!(loaded.inert_legacy_count, 1);
    assert_eq!(fixture.pending(), 0);
}
