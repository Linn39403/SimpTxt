//! Real-file integration coverage, with a bounded-memory expected-byte oracle.
//!
//! Regular tests use 1 MB. Explicit opt-in tests cover 10 MB, 100 MB, and exactly
//! 1,000,000,000 bytes. For the 1 GB test, SIMPTXT_FIXTURE_FAMILY selects source
//! (default), logs, or long. Run each family separately to bound disk use:
//!
//! cargo test --release --no-default-features --test large_file \
//!     one_gigabyte_open_edit_save -- --ignored --exact --nocapture
//!
//! The temporary directory needs about twice the fixture size during a save.
//! Only one loaded document is retained, including during reopen verification.
//! Timings describe core operations, NOT rendered UI latency. Generated fixtures
//! have uncontrolled/warm caches; this harness does not measure cold-cache p95.
//! See REQUIREMENTS.md for the separate 20-trial, hardware-recorded UI plan.

use simptxt::document::Document;
use simptxt::file_io::{self, FileStamp, IO_CHUNK_BYTES};
use simptxt::search;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const ONE_GIGABYTE: usize = 1_000_000_000;
const BUFFER_BYTES: usize = 256 * 1024;

// Keep these seeds identical to scripts/generate_fixtures.py. Source exercises
// BOM, mixed line endings, multiline lexical state, and multibyte boundaries.
const SOURCE: &str = concat!(
    "/* source fixture: a multiline comment\n",
    "second comment line: café λ 世界 🙂 */\r\n",
    "fn example() {\n",
    "    let greeting = \"hello, λ 世界 🙂\";\r\n",
    "    let multiline = \"first line\n",
    "second line\";\n",
    "    println!(\"{greeting}: {multiline}\");\r\n",
    "}\n",
);
const LOGS: &str = "2026-01-01T00:00:00Z INFO request=42 path=/home status=200\r\n\
2026-01-01T00:00:01Z INFO user=café message=世界🙂 duration_ms=17\n\
2026-01-01T00:00:02Z WARN retry=1 reason=λ_timeout\r\n";
const LONG: &str = "0123456789abcdef café λ 世界 🙂 single_line_no_wrap ";

#[derive(Clone, Copy, Debug)]
enum Family {
    Source,
    Logs,
    Long,
}

impl Family {
    fn name(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Logs => "logs",
            Self::Long => "long",
        }
    }

    fn seed(self) -> &'static str {
        match self {
            Self::Source => SOURCE,
            Self::Logs => LOGS,
            Self::Long => LONG,
        }
    }

    fn prefix(self) -> &'static [u8] {
        match self {
            Self::Source => b"\xef\xbb\xbf",
            Self::Logs | Self::Long => b"",
        }
    }

    fn from_environment() -> Self {
        match std::env::var("SIMPTXT_FIXTURE_FAMILY")
            .unwrap_or_else(|_| "source".to_owned())
            .as_str()
        {
            "source" => Self::Source,
            "logs" => Self::Logs,
            "long" => Self::Long,
            value => panic!("Unknown SIMPTXT_FIXTURE_FAMILY {value:?}; use source, logs, or long"),
        }
    }
}

/// A random-access expected fixture, occupying at most one bounded block.
/// A final incomplete UTF-8 codepoint is replaced with ASCII spaces, not a
/// replacement character or an invalid partial encoding.
struct Fixture {
    family: Family,
    len: usize,
    repeating_len: usize,
    block: Vec<u8>,
}

impl Fixture {
    fn new(family: Family, len: usize) -> Self {
        assert!(len >= family.prefix().len());
        let seed = family.seed().as_bytes();
        let body_len = len - family.prefix().len();
        let tail_len = body_len % seed.len();
        let valid_tail_len = match std::str::from_utf8(&seed[..tail_len]) {
            Ok(_) => tail_len,
            Err(error) => {
                assert!(error.error_len().is_none());
                error.valid_up_to()
            }
        };
        Self {
            family,
            len,
            repeating_len: body_len - tail_len + valid_tail_len,
            block: seed.repeat((BUFFER_BYTES / seed.len()).max(1)),
        }
    }

    fn fill(&self, mut offset: usize, output: &mut [u8]) {
        assert!(offset <= self.len && output.len() <= self.len - offset);
        let prefix = self.family.prefix();
        let mut written = 0;
        if offset < prefix.len() {
            let take = output.len().min(prefix.len() - offset);
            output[..take].copy_from_slice(&prefix[offset..offset + take]);
            written += take;
            offset += take;
        }
        while written < output.len() {
            let body_offset = offset - prefix.len();
            if body_offset >= self.repeating_len {
                output[written..].fill(b' ');
                break;
            }
            let block_offset = body_offset % self.block.len();
            let take = (output.len() - written)
                .min(self.block.len() - block_offset)
                .min(self.repeating_len - body_offset);
            output[written..written + take]
                .copy_from_slice(&self.block[block_offset..block_offset + take]);
            written += take;
            offset += take;
        }
    }

    fn write(&self, path: &Path) {
        let mut file = File::create(path).expect("create fixture");
        let mut buffer = vec![0; BUFFER_BYTES];
        let mut offset = 0;
        while offset < self.len {
            let take = buffer.len().min(self.len - offset);
            self.fill(offset, &mut buffer[..take]);
            file.write_all(&buffer[..take]).expect("write fixture");
            offset += take;
        }
        file.sync_all().expect("flush fixture");
        assert_eq!(file.metadata().unwrap().len(), self.len as u64);
    }
}

#[derive(Clone)]
struct Edit {
    original_range: Range<usize>,
    replacement: &'static str,
}

enum Segment<'a> {
    Original(Range<usize>),
    Inserted(&'a [u8]),
}

/// Generates edited expected bytes as a stream, never a second large document.
struct Expected<'a> {
    fixture: &'a Fixture,
    segments: Vec<Segment<'a>>,
    segment: usize,
    within: usize,
}

impl<'a> Expected<'a> {
    fn new(fixture: &'a Fixture, edits: &'a [Edit]) -> Self {
        let mut segments = Vec::new();
        let mut previous_end = 0;
        for edit in edits {
            assert!(edit.original_range.start >= previous_end);
            assert!(edit.original_range.end <= fixture.len);
            segments.push(Segment::Original(previous_end..edit.original_range.start));
            segments.push(Segment::Inserted(edit.replacement.as_bytes()));
            previous_end = edit.original_range.end;
        }
        segments.push(Segment::Original(previous_end..fixture.len));
        Self {
            fixture,
            segments,
            segment: 0,
            within: 0,
        }
    }
}

impl Read for Expected<'_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let mut written = 0;
        while written < output.len() && self.segment < self.segments.len() {
            let length = match &self.segments[self.segment] {
                Segment::Original(range) => range.len(),
                Segment::Inserted(bytes) => bytes.len(),
            };
            if self.within == length {
                self.segment += 1;
                self.within = 0;
                continue;
            }
            let take = (output.len() - written).min(length - self.within);
            match &self.segments[self.segment] {
                Segment::Original(range) => self.fixture.fill(
                    range.start + self.within,
                    &mut output[written..written + take],
                ),
                Segment::Inserted(bytes) => output[written..written + take]
                    .copy_from_slice(&bytes[self.within..self.within + take]),
            }
            self.within += take;
            written += take;
        }
        Ok(written)
    }
}

fn assert_stream_matches(mut actual: impl Read, fixture: &Fixture, edits: &[Edit]) {
    let mut expected = Expected::new(fixture, edits);
    let mut expected_bytes = vec![0; BUFFER_BYTES];
    let mut actual_bytes = vec![0; BUFFER_BYTES];
    let mut offset = 0usize;
    loop {
        let count = expected.read(&mut expected_bytes).unwrap();
        if count == 0 {
            break;
        }
        actual
            .read_exact(&mut actual_bytes[..count])
            .unwrap_or_else(|error| panic!("Short file near byte {offset}: {error}"));
        if let Some(index) = actual_bytes[..count]
            .iter()
            .zip(&expected_bytes[..count])
            .position(|(actual, expected)| actual != expected)
        {
            panic!(
                "Byte mismatch at {}: actual={:#04x}, expected={:#04x}",
                offset + index,
                actual_bytes[index],
                expected_bytes[index]
            );
        }
        offset += count;
    }
    let mut extra = [0];
    assert_eq!(
        actual.read(&mut extra).unwrap(),
        0,
        "Unexpected trailing data"
    );
}

fn assert_original_samples(document: &Document, fixture: &Fixture) {
    assert_eq!(document.len(), fixture.len);
    for center in [0, fixture.len / 2, fixture.len] {
        let start = center.saturating_sub(64);
        let end = (center + 64).min(fixture.len);
        let mut expected = vec![0; end - start];
        fixture.fill(start, &mut expected);
        assert_eq!(document.read(start..end), expected);
        let line = document.line_of_offset(center);
        assert!(document.line_start(line) <= center);
    }
    if matches!(fixture.family, Family::Long) {
        assert_eq!(document.line_count(), 1);
        assert_eq!(document.line_start(0), 0);
        assert_eq!(document.line_start(1), fixture.len);
    }
}

fn edits_for(document: &Document, fixture: &Fixture) -> Vec<Edit> {
    let mut middle_start = document.len() / 2;
    while !document.char_boundary(middle_start) {
        middle_start += 1;
    }
    let mut middle_end = middle_start + 17;
    while !document.char_boundary(middle_end) {
        middle_end += 1;
    }
    vec![
        Edit {
            original_range: fixture.family.prefix().len()..fixture.family.prefix().len(),
            replacement: "BEGIN_EDIT_é🙂 ",
        },
        Edit {
            original_range: middle_start..middle_end,
            replacement: "MIDDLE_EDIT_λ中 ",
        },
        Edit {
            original_range: document.len()..document.len(),
            replacement: " END_EDIT_ö🚀",
        },
    ]
}

fn expected_len(fixture: &Fixture, edits: &[Edit]) -> usize {
    edits.iter().fold(fixture.len, |length, edit| {
        length - edit.original_range.len() + edit.replacement.len()
    })
}

fn assert_edited_samples(document: &Document, fixture: &Fixture, edits: &[Edit]) {
    assert_eq!(document.len(), expected_len(fixture, edits));
    let mut original_end = 0;
    let mut edited_end = 0;
    for edit in edits {
        let start = edited_end + edit.original_range.start - original_end;
        let end = start + edit.replacement.len();
        assert_eq!(document.read(start..end), edit.replacement.as_bytes());
        assert_eq!(
            search::search(
                document,
                edit.replacement,
                start,
                false,
                &AtomicBool::new(false),
                &AtomicUsize::new(0),
            )
            .expect("find inserted UTF-8 text"),
            Some(start..end)
        );
        original_end = edit.original_range.end;
        edited_end = end;
    }
    assert_eq!(
        document.read(0..fixture.family.prefix().len()),
        fixture.family.prefix()
    );
}

fn metric(fixture: &Fixture, case: &str, duration: Duration, bytes: usize) {
    let mb_per_second = bytes as f64 / 1_000_000.0 / duration.as_secs_f64();
    eprintln!(
        "METRIC family={} fixture_bytes={} case={} elapsed_ns={} bytes={} decimal_MB_per_s={:.3}",
        fixture.family.name(),
        fixture.len,
        case,
        duration.as_nanos(),
        bytes,
        mb_per_second
    );
}

/// A panic in the save implementation must not strand the observer and prevent
/// the scoped thread from joining during test unwinding.
struct CompletionGuard<'a>(&'a AtomicBool);

impl Drop for CompletionGuard<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// For the large fixtures, cancel only after the public progress counter proves
/// that bytes have actually been written to the temporary replacement.
fn verify_cancelled_save(document: &Document, path: &Path, stamp: &FileStamp, fixture: &Fixture) {
    let cancel = AtomicBool::new(false);
    let progress = AtomicU64::new(0);
    let start = Instant::now();
    if fixture.len >= 10_000_000 {
        let done = AtomicBool::new(false);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(0);
        let (result, requested_at) = thread::scope(|scope| {
            let completion = CompletionGuard(&done);
            let observer = scope.spawn(|| {
                ready_sender.send(()).unwrap();
                while progress.load(Ordering::Relaxed) < IO_CHUNK_BYTES as u64 {
                    if done.load(Ordering::Acquire) {
                        return None;
                    }
                    thread::yield_now();
                }
                let requested_at = Instant::now();
                cancel.store(true, Ordering::Relaxed);
                Some(requested_at)
            });
            ready_receiver.recv().unwrap();
            let result = file_io::save(document, path, Some(stamp), &cancel, &progress);
            let finished_at = Instant::now();
            drop(completion);
            let requested_at = observer.join().expect("cancellation observer");
            (result, requested_at.map(|request| (request, finished_at)))
        });
        let error = result.expect_err("an in-progress save must acknowledge cancellation");
        assert!(error.to_lowercase().contains("cancel"), "{error}");
        let (requested_at, finished_at) =
            requested_at.expect("cancellation was requested after a real write");
        assert!(progress.load(Ordering::Relaxed) >= IO_CHUNK_BYTES as u64);
        metric(
            fixture,
            "save_cancellation_acknowledgment",
            finished_at.duration_since(requested_at),
            0,
        );
    } else {
        cancel.store(true, Ordering::Relaxed);
        let error = file_io::save(document, path, Some(stamp), &cancel, &progress)
            .expect_err("pre-cancelled save");
        assert!(error.to_lowercase().contains("cancel"), "{error}");
        assert_eq!(progress.load(Ordering::Relaxed), 0);
    }
    metric(
        fixture,
        "cancelled_save_total",
        start.elapsed(),
        progress.load(Ordering::Relaxed) as usize,
    );
    assert_eq!(FileStamp::read(path).unwrap(), *stamp);
    assert_stream_matches(File::open(path).unwrap(), fixture, &[]);
    assert!(
        document.is_modified(),
        "failed save must retain unsaved edits"
    );
    assert_eq!(
        fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1,
        "cancelled save must remove its temporary file"
    );
}

fn run_pipeline(family: Family, bytes: usize) {
    let directory = tempfile::tempdir().expect("temporary fixture directory");
    let path = directory.path().join("fixture.txt");
    let fixture = Fixture::new(family, bytes);
    fixture.write(&path);
    assert_eq!(fs::metadata(&path).unwrap().len(), bytes as u64);
    eprintln!(
        "RUN family={} actual_fixture_bytes={} trials=1 cache=uncontrolled_generated_fixture \
         measurements=core_not_UI no_percentile_claim=true",
        family.name(),
        bytes
    );

    let open_started = Instant::now();
    let preview = file_io::preview(&path, 4096).expect("bounded read-only preview");
    metric(&fixture, "first_preview_read", open_started.elapsed(), 4096);
    assert!(preview.contains("[Read-only preview:"));
    assert!(preview.contains(&family.seed()[..32]));
    assert!(preview.contains("[Preview ends here;"));
    let load_started = Instant::now();
    let progress = AtomicU64::new(0);
    let loaded = file_io::load(&path, &AtomicBool::new(false), &progress).expect("real file load");
    metric(&fixture, "full_load", load_started.elapsed(), bytes);
    metric(
        &fixture,
        "editing_ready_since_open",
        open_started.elapsed(),
        bytes,
    );
    assert_eq!(progress.load(Ordering::Relaxed), bytes as u64);
    let mut document = loaded.document;
    let mut stamp = loaded.stamp;
    assert!(!document.is_modified());
    assert_original_samples(&document, &fixture);

    let save_started = Instant::now();
    stamp = file_io::save(
        &document,
        &path,
        Some(&stamp),
        &AtomicBool::new(false),
        &progress,
    )
    .expect("byte-preserving no-op save");
    metric(&fixture, "noop_save", save_started.elapsed(), bytes);
    assert_eq!(progress.load(Ordering::Relaxed), bytes as u64);
    assert_stream_matches(File::open(&path).unwrap(), &fixture, &[]);

    let edits = edits_for(&document, &fixture);
    for (index, edit) in edits.iter().enumerate().rev() {
        let started = Instant::now();
        document
            .replace(edit.original_range.clone(), edit.replacement)
            .expect("small UTF-8 edit");
        metric(
            &fixture,
            ["edit_beginning", "edit_middle", "edit_end"][index],
            started.elapsed(),
            0,
        );
    }
    assert!(document.is_modified());
    assert_edited_samples(&document, &fixture, &edits);
    for _ in &edits {
        let started = Instant::now();
        assert!(document.undo());
        metric(&fixture, "undo_one_edit", started.elapsed(), 0);
    }
    assert!(!document.undo());
    assert!(!document.is_modified());
    assert_original_samples(&document, &fixture);
    for _ in &edits {
        let started = Instant::now();
        assert!(document.redo());
        metric(&fixture, "redo_one_edit", started.elapsed(), 0);
    }
    assert!(!document.redo());
    assert!(document.is_modified());
    assert_edited_samples(&document, &fixture, &edits);
    verify_cancelled_save(&document, &path, &stamp, &fixture);

    let saved_len = expected_len(&fixture, &edits);
    let save_started = Instant::now();
    let saved = file_io::save(
        &document,
        &path,
        Some(&stamp),
        &AtomicBool::new(false),
        &progress,
    )
    .expect("streamed edited save");
    metric(&fixture, "edited_save", save_started.elapsed(), saved_len);
    assert_eq!(saved.len, saved_len as u64);
    assert_eq!(progress.load(Ordering::Relaxed), saved_len as u64);
    document.mark_saved();
    assert!(!document.is_modified());
    assert_stream_matches(File::open(&path).unwrap(), &fixture, &edits);

    // Do not retain two 1 GB documents merely to prove reopening works.
    drop(document);
    let reopened_at = Instant::now();
    let reopened =
        file_io::load(&path, &AtomicBool::new(false), &progress).expect("reopen edited file");
    metric(
        &fixture,
        "reopen_edited_file",
        reopened_at.elapsed(),
        saved_len,
    );
    assert_eq!(reopened.stamp, saved);
    assert_eq!(progress.load(Ordering::Relaxed), saved_len as u64);
    assert!(!reopened.document.is_modified());
    assert_edited_samples(&reopened.document, &fixture, &edits);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    drop(reopened);
    // tempfile removes the fixture here, including if an assertion unwinds.
}

#[test]
fn one_megabyte_source_open_edit_save() {
    run_pipeline(Family::Source, 1_000_000);
}

#[test]
fn one_megabyte_logs_open_edit_save() {
    run_pipeline(Family::Logs, 1_000_000);
}

#[test]
fn one_megabyte_single_line_open_edit_save() {
    run_pipeline(Family::Long, 1_000_000);
}

#[test]
#[ignore = "opt-in 10 MB real-file fixtures; use --ignored --exact"]
fn ten_megabyte_all_families_open_edit_save() {
    for family in [Family::Source, Family::Logs, Family::Long] {
        run_pipeline(family, 10_000_000);
    }
}

#[test]
#[ignore = "opt-in 100 MB real-file fixtures; use --ignored --exact"]
fn hundred_megabyte_all_families_open_edit_save() {
    for family in [Family::Source, Family::Logs, Family::Long] {
        run_pipeline(family, 100_000_000);
    }
}

#[test]
#[ignore = "writes a real 1,000,000,000-byte fixture; needs about 2 GB free disk"]
fn one_gigabyte_open_edit_save() {
    run_pipeline(Family::from_environment(), ONE_GIGABYTE);
}

#[test]
fn fixture_tail_and_streaming_edit_oracle_are_valid() {
    for family in [Family::Source, Family::Logs, Family::Long] {
        for length in 3..family.seed().len() * 2 + 3 {
            let fixture = Fixture::new(family, length);
            let mut bytes = vec![0; length];
            fixture.fill(0, &mut bytes);
            std::str::from_utf8(&bytes).expect("fixture tail must be valid UTF-8");
            assert!(!bytes.contains(&0));
            if matches!(family, Family::Long) {
                assert!(!bytes.contains(&b'\n'));
            }
            assert_stream_matches(bytes.as_slice(), &fixture, &[]);
        }
    }
    let fixture = Fixture::new(Family::Logs, 1000);
    let edits = [
        Edit {
            original_range: 0..0,
            replacement: "🙂",
        },
        Edit {
            original_range: 4..11,
            replacement: "é",
        },
        Edit {
            original_range: 1000..1000,
            replacement: "end",
        },
    ];
    let mut reference = vec![0; fixture.len];
    fixture.fill(0, &mut reference);
    for edit in edits.iter().rev() {
        reference.splice(edit.original_range.clone(), edit.replacement.bytes());
    }
    assert_stream_matches(reference.as_slice(), &fixture, &edits);
}
