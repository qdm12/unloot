//! End-to-end tests against the real binary: flags, environment, files on
//! disk, exit codes. They never touch the network — `--mock-llm` answers the
//! two prompts (boundaries, then one per item) from a fixture.
//!
//! Photos are synthesised rather than read from `input/`, so the tests keep
//! working after a real run has emptied that folder.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use image::{ExtendedColorType, ImageBuffer, ImageEncoder, Rgb};
use tempfile::TempDir;

/// A 1600x1200 photo, noisy enough that JPEG quality actually has to work,
/// with an optional hand-written EXIF APP1 segment carrying Orientation=6
/// ("rotate 90 clockwise", so the displayed image is 1200x1600).
fn write_photo(path: &Path, seed: u32, with_exif: bool) {
    let mut buf = Vec::new();
    let img: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_fn(1600, 1200, |x, y| {
        let n = (x * 7 + y * 11 + seed * 37) % 251;
        Rgb([(n * 3) as u8, (n * 5 + 40) as u8, (n * 7 + 90) as u8])
    });
    let mut cur = std::io::Cursor::new(&mut buf);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cur, 95)
        .write_image(img.as_raw(), 1600, 1200, ExtendedColorType::Rgb8)
        .unwrap();
    // Uncompressed enough to be over the 300 KiB ceiling once re-encoded small.
    if with_exif {
        buf.splice(2..2, exif_app1_orientation_6());
    }
    fs::write(path, buf).unwrap();
}

fn exif_app1_orientation_6() -> Vec<u8> {
    let mut seg = vec![0xFF, 0xE1]; // APP1
    let body: Vec<u8> = b"Exif\0\0"
        .iter()
        .copied()
        .chain([
            b'I', b'I', 0x2A, 0x00, // little-endian TIFF
            0x08, 0x00, 0x00, 0x00, // first IFD at offset 8
            0x01, 0x00, // one entry
            0x12, 0x01, 0x03, 0x00, // tag 0x0112 Orientation, type SHORT
            0x01, 0x00, 0x00, 0x00, // count 1
            0x06, 0x00, 0x00, 0x00, // value 6
            0x00, 0x00, 0x00, 0x00, // no next IFD
        ])
        .collect();
    let len = (body.len() + 2) as u16;
    seg.push((len >> 8) as u8);
    seg.push(len as u8);
    seg.extend(body);
    seg
}

/// Six photos, three of one item and three of another.
fn make_input(dir: &Path) -> Vec<String> {
    let names = [
        "PXL_20261004_150740746.jpg",
        "PXL_20261004_150745194.jpg",
        "PXL_20261004_150751450.jpg",
        "PXL_20261004_150905478.jpg",
        "PXL_20261004_150914119.jpg",
        "PXL_20261004_150929457.jpg",
    ];
    for (i, n) in names.iter().enumerate() {
        write_photo(&dir.join(n), i as u32, i == 0);
    }
    names.iter().map(|s| s.to_string()).collect()
}

/// Two items of three photos each, then one description per item.
const FIXTURE: &str = r#"{
  "calls": [
    {"images": [
      {"id": 1, "verdict": "new",  "subject": "grey cotton t-shirt"},
      {"id": 2, "verdict": "same", "subject": "grey cotton t-shirt"},
      {"id": 3, "verdict": "same", "subject": "grey cotton t-shirt"},
      {"id": 4, "verdict": "new",  "subject": "blue denim jeans"},
      {"id": 5, "verdict": "same", "subject": "blue denim jeans"},
      {"id": 6, "verdict": "same", "subject": "blue denim jeans"}
    ]},
    {"category": "T-shirt", "brand": "Uniqlo", "color": "grey", "size": "M", "price": 9.5, "weight_g": 180},
    {"category": "Jeans", "brand": "Levi's", "color": "blue", "size": "32", "details": "slim leg", "price": 24, "weight_g": 600}
  ]
}"#;

struct Run {
    root: TempDir,
    input: PathBuf,
    output: PathBuf,
    fixture: PathBuf,
}

impl Run {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("in");
        let output = root.path().join("out");
        fs::create_dir_all(&input).unwrap();
        fs::create_dir_all(&output).unwrap();
        make_input(&input);
        let fixture = root.path().join("fixture.json");
        fs::write(&fixture, FIXTURE).unwrap();
        Self {
            root,
            input,
            output,
            fixture,
        }
    }

    /// `env` entries are set for the child; anything starting with UNLOOT_ not
    /// mentioned is removed, so the developer's shell cannot leak in.
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_unloot"));
        // The fixture replays its answers in call order, and items are described
        // in parallel by default — which would let item B pick up item A's
        // description. Serialise so the fixture means what it says.
        let mut argv: Vec<&str> = vec!["--jobs", "1"];
        argv.extend(args);
        cmd.arg("--input-dir")
            .arg(&self.input)
            .arg("--output-dir")
            .arg(&self.output)
            .arg("--mock-llm")
            .arg("--mock-llm-file")
            .arg(&self.fixture)
            .arg("--llm-url")
            .arg("http://127.0.0.1:1/v1"); // never contacted
        cmd.env_remove("UNLOOT_INPUT_DIR")
            .env_remove("UNLOOT_OUTPUT_DIR")
            .env_remove("UNLOOT_MAX_LONG_EDGE")
            .env_remove("UNLOOT_MAX_BYTES")
            .env_remove("UNLOOT_KEEP_SOURCE")
            .env_remove("UNLOOT_RESUME")
            .env_remove("UNLOOT_CURRENCY")
            .env_remove("UNLOOT_LOG");
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.args(&argv);
        let out = cmd.output().expect("run unloot");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            panic!("unloot {args:?} failed: {stderr}\n{stdout}");
        }
        out
    }

    /// A quieter variant for the tests that expect failure.
    fn try_run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_unloot"));
        cmd.arg("--input-dir")
            .arg(&self.input)
            .arg("--output-dir")
            .arg(&self.output)
            .env_remove("UNLOOT_LLM_URL");
        {
            let (k, v) = ("UNLOOT_LLM_URL", "");
            cmd.env(k, v);
        }
        cmd.args(args).output().expect("run unloot")
    }

    /// Item folders only: the dot-prefixed `.unloot` working area is ours, not
    /// an item.
    fn dirs(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(&self.output)
            .unwrap()
            .flatten()
            .filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('.'))
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        v.sort();
        v
    }

    fn csv(&self) -> String {
        let e = fs::read_dir(&self.output)
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().starts_with("summary_"))
            .expect("a summary CSV");
        fs::read_to_string(e.path()).unwrap()
    }

    /// A shelf of photos already sorted into folders by hand, or on another
    /// machine: the same directory is the input and the output, and the only
    /// thing missing is the summary. Answers are replayed in folder-name order.
    fn sorted(fixture: &str, folders: &[&str]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let shelf = root.path().join("shelf");
        fs::create_dir_all(&shelf).unwrap();
        for (i, folder) in folders.iter().enumerate() {
            let dir = shelf.join(folder);
            fs::create_dir_all(&dir).unwrap();
            write_photo(
                &dir.join(format!("PXL_20261004_15074{i}746.jpg")),
                i as u32,
                false,
            );
            write_photo(
                &dir.join(format!("PXL_20261004_15074{i}891.jpg")),
                i as u32 + 9,
                false,
            );
        }
        let fixture_path = root.path().join("fixture.json");
        fs::write(&fixture_path, fixture).unwrap();
        Self {
            root,
            input: shelf.clone(),
            output: shelf,
            fixture: fixture_path,
        }
    }

    /// Every file below the output directory, by path and size: a folder that
    /// was left alone has the same snapshot before and after.
    /// Every file below the output directory except unloot's own bookkeeping and
    /// the summaries it writes: a shelf that was left alone has the same
    /// snapshot before and after.
    fn snapshot(&self) -> Vec<String> {
        fn walk(dir: &Path, prefix: &str, top: bool, out: &mut Vec<String>) {
            for entry in fs::read_dir(dir).unwrap().flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let path = entry.path();
                if top && (name.starts_with('.') || name.starts_with("summary_")) {
                    continue;
                }
                let label = format!("{prefix}{name}");
                if path.is_dir() {
                    walk(&path, &format!("{label}/"), false, out);
                } else {
                    out.push(format!("{label} {}", entry.metadata().unwrap().len()));
                }
            }
        }
        let mut v = Vec::new();
        walk(&self.output, "", true, &mut v);
        v.sort();
        v
    }

    fn summaries(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(&self.output)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("summary_"))
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        v.sort();
        v
    }

    fn photos(&self, dir: &str) -> Vec<PathBuf> {
        let mut v = fs::read_dir(self.output.join(dir))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jpg"))
            .collect::<Vec<_>>();
        v.sort();
        v
    }
}

#[test]
fn dry_run_touches_nothing_and_reports_what_it_would_do() {
    let r = Run::new();
    let out = r.run(&["--dry-run"], &[]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(stdout.contains("T-shirt Uniqlo grey size M"), "{stdout}");
    assert!(
        stdout.contains("Jeans Levi's slim leg blue size 32"),
        "{stdout}"
    );
    // Nothing was created but the lock/ledger scaffolding at most.
    assert!(
        r.dirs().is_empty(),
        "dry run created folders: {:?}",
        r.dirs()
    );
    assert!(
        fs::read_dir(&r.input).unwrap().count() == 6,
        "dry run deleted sources"
    );
}

#[test]
fn one_folder_per_item_within_the_size_limits_with_the_sources_gone() {
    let r = Run::new();
    let out = r.run(&["--log", "debug"], &[]);
    let log = String::from_utf8_lossy(&out.stdout).to_string();
    std::fs::write("/var/tmp/unloot-child.log", &log).unwrap();
    assert_eq!(
        r.dirs(),
        vec![
            "Jeans Levi's slim leg blue size 32",
            "T-shirt Uniqlo grey size M"
        ]
    );
    assert_eq!(r.photos("T-shirt Uniqlo grey size M").len(), 3);
    assert_eq!(r.photos("Jeans Levi's slim leg blue size 32").len(), 3);

    for d in r.dirs() {
        for p in r.photos(&d) {
            let bytes = fs::read(&p).unwrap();
            assert!(
                bytes.len() <= 300 * 1024,
                "{} is {} bytes",
                p.display(),
                bytes.len()
            );
            let img = image::load_from_memory(&bytes).unwrap();
            assert!(
                img.width() <= 1280 && img.height() <= 1280,
                "{} is {}x{}",
                p.display(),
                img.width(),
                img.height()
            );
            // The first photo went in with Orientation=6; the copy must carry
            // no EXIF at all, and must have that rotation baked into its pixels.
            assert!(
                !bytes.windows(6).any(|w| w == b"Exif\0\0"),
                "{} still has EXIF",
                p.display()
            );
        }
    }
    // Which way up the pixels ended up is also checked in the unit tests, where
    // the source size is known exactly; here the byte ceiling shrinks each photo
    // by an amount of its own. What is asserted here is that the first photo of
    // the first item went in landscape-with-Orientation-6 and came out portrait.
    let first = r.photos("T-shirt Uniqlo grey size M")[0].clone();
    let img = image::open(&first).unwrap();
    let all: Vec<String> = r
        .photos("T-shirt Uniqlo grey size M")
        .iter()
        .map(|p| {
            let i = image::open(p).unwrap();
            format!(
                "{} {}x{}",
                p.file_name().unwrap().to_string_lossy(),
                i.width(),
                i.height()
            )
        })
        .collect();
    assert!(
        img.height() > img.width(),
        "orientation not baked on {}: {}x{} — item holds [{}])\n{log}",
        first.display(),
        img.width(),
        img.height(),
        all.join(" | ")
    );

    // The originals are gone by default; the ledger says where they went.
    assert_eq!(fs::read_dir(&r.input).unwrap().count(), 0);
    let ledger = fs::read_to_string(r.root.path().join("out/.unloot/state.json")).unwrap();
    assert!(ledger.contains("T-shirt Uniqlo grey size M"), "{ledger}");

    let csv = r.csv();
    assert!(
        csv.starts_with("name,category,brand,color,size,price (USD),weight (oz)\n"),
        "{csv}"
    );
    // Whole currency units, never cents: 9.5 became 10, not 9.50.
    assert!(
        csv.contains("T-shirt Uniqlo grey size M,T-shirt,Uniqlo,grey,M,10,6.3"),
        "{csv}"
    );
    assert!(
        csv.contains("Jeans Levi's slim leg blue size 32,Jeans,Levi's,blue,32,24,21.2"),
        "{csv}"
    );
    assert_eq!(
        csv.trim().lines().count(),
        3,
        "one header and two items: {csv}"
    );
}

#[test]
fn keep_source_leaves_the_originals_alone() {
    let r = Run::new();
    r.run(&["--keep-source"], &[]);
    assert_eq!(fs::read_dir(&r.input).unwrap().count(), 6);
    assert_eq!(r.dirs().len(), 2);
}

#[test]
fn environment_is_honoured_and_a_flag_beats_it() {
    let r = Run::new();
    r.run(&[], &[("UNLOOT_MAX_LONG_EDGE", "640")]);
    let img = image::open(&r.photos("T-shirt Uniqlo grey size M")[1]).unwrap();
    assert!(
        img.width().max(img.height()) <= 640,
        "env not applied: {}x{}",
        img.width(),
        img.height()
    );

    let r2 = Run::new();
    r2.run(
        &["--max-long-edge", "800"],
        &[("UNLOOT_MAX_LONG_EDGE", "640")],
    );
    let img = image::open(&r2.photos("T-shirt Uniqlo grey size M")[1]).unwrap();
    // 1600x1200 with the long edge capped at 800, the rotation baked in last
    // for the first photo only; the second is landscape.
    assert_eq!(
        img.width(),
        800,
        "flag did not beat the environment: {}x{}",
        img.width(),
        img.height()
    );
}

#[test]
fn a_second_run_with_the_ledger_does_nothing_twice() {
    let r = Run::new();
    r.run(&["--keep-source"], &[]);
    let before = r.dirs();
    let out = r.run(&["--keep-source"], &[]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(
        r.dirs(),
        before,
        "the second run made new folders: {stdout}"
    );
    assert!(
        stdout.contains("no photos to process") || stdout.contains("0 item"),
        "{stdout}"
    );
}

#[test]
fn resume_can_be_switched_off_and_then_it_would_start_over() {
    let r = Run::new();
    r.run(&["--keep-source"], &[]);
    // --no-resume means the ledger is ignored, so the same photos come back as
    // numbered folders rather than silently vanishing.
    let out = r.run(&["--keep-source", "--no-resume"], &[]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        r.dirs().len() > 2,
        "expected numbered duplicates, got {:?} ({stdout})",
        r.dirs()
    );
    assert!(
        r.dirs().iter().any(|d| d.ends_with(" (2)")),
        "{:?}",
        r.dirs()
    );
}

#[test]
fn currency_drives_both_header_names() {
    let r = Run::new();
    r.run(&[], &[("UNLOOT_CURRENCY", "EUR")]);
    let csv = r.csv();
    assert!(csv.contains("price (EUR)"), "{csv}");
    assert!(csv.contains("weight (g)"), "{csv}");
    // Weight is the last column, so the grams value ends the line rather than
    // sitting between commas.
    assert!(
        !csv.contains(",6.3"),
        "ounces leaked into a grams column: {csv}"
    );
    let tshirt = csv
        .lines()
        .find(|l| l.starts_with("T-shirt "))
        .unwrap_or_default();
    assert!(
        tshirt.ends_with(",180"),
        "expected grams for the t-shirt: {csv}"
    );
}

#[test]
fn a_missing_endpoint_is_refused_before_anything_is_touched() {
    let r = Run::new();
    let out = r.try_run(&[]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        stderr.contains("llm-url") || stderr.contains("llm_url"),
        "{stderr}"
    );
    assert!(r.dirs().is_empty());
    assert_eq!(fs::read_dir(&r.input).unwrap().count(), 6);
}

#[test]
fn the_output_directory_is_created_when_it_does_not_exist() {
    let r = Run::new();
    let nested = r.root.path().join("deep/nested/out");
    let out = Command::new(env!("CARGO_BIN_EXE_unloot"))
        .arg("--input-dir")
        .arg(&r.input)
        .arg("--output-dir")
        .arg(&nested)
        .arg("--mock-llm")
        .arg("--mock-llm-file")
        .arg(&r.fixture)
        .arg("--llm-url")
        .arg("http://127.0.0.1:1/v1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(nested.is_dir());
}

/// Two folders of photos, already sorted by hand or on another machine. The
/// names are theirs; unloot's job is only to fill in the summary.
const SORTED_FIXTURE: &str = r#"{
  "calls": [
    {"category":"Jeans","brand":"Levi's","color":"blue","size":"32","details":"slim leg","price":23.6,"weight_g":600},
    {"category":"Sneakers","brand":"Adidas","color":"white","size":"43","details":null,"price":31.2,"weight_g":700}
  ]
}"#;

/// The summary gains a row per folder, named after the folder and not after
/// anything the model would have invented, and the folders themselves are not
/// touched — not one byte, not one name.
#[test]
fn describes_folders_that_are_already_sorted() {
    let r = Run::sorted(
        SORTED_FIXTURE,
        &["Jeans blue size 32", "Sneakers white size 43"],
    );
    let before = r.snapshot();

    let out = r.run(&[], &[]);
    let log = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        log.contains("2 described where they stand"),
        "both folders should be described: {log}"
    );
    assert!(
        log.contains("0 photo(s)"),
        "describing a shelf should not re-encode anything: {log}"
    );
    let csv = r.csv();
    assert!(
        csv.starts_with("name,category,brand,color,size,price (USD),weight (oz)\n"),
        "{csv}"
    );
    assert_eq!(
        csv.trim().lines().count(),
        3,
        "one header and the two folders: {csv}"
    );
    // Whole currency units, and the folder's own name in the first column.
    assert!(
        csv.contains("Jeans blue size 32,Jeans,Levi's,blue,32,24,21.2"),
        "{csv}"
    );
    assert!(
        csv.contains("Sneakers white size 43,Sneakers,Adidas,white,43,31,24.7"),
        "{csv}"
    );
    assert_eq!(
        r.snapshot(),
        before,
        "the folders must be left exactly as found"
    );

    // A second look finds nothing new, because the summary already lists them.
    let out = r.run(&[], &[]);
    let log = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(log.contains("nothing to report"), "{log}");
    assert_eq!(
        r.summaries().len(),
        1,
        "the second run should not add a summary: {:?}",
        r.summaries()
    );
    assert_eq!(r.snapshot(), before);
}

/// Describing folders is a courtesy to a shelf that is already sorted; switch it
/// off and an output directory of folders alone is nothing to do.
#[test]
fn describing_existing_folders_can_be_switched_off() {
    let r = Run::sorted(
        SORTED_FIXTURE,
        &["Jeans blue size 32", "Sneakers white size 43"],
    );
    let before = r.snapshot();
    let out = r.run(&["--no-describe-dirs"], &[]);
    let log = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(log.contains("nothing to report"), "{log}");
    assert!(r.summaries().is_empty(), "no summary should be written");
    assert_eq!(r.snapshot(), before);
}
