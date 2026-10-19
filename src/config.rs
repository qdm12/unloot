//! Option definitions, precedence (flag > env > default) and resolution.
//!
//! [`OPTIONS`] is the single source of truth: it drives `clap`, the `--help`
//! text, `--print-config` provenance and `--print-options-md` (the README
//! table). Adding an option here makes it exist everywhere.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Arg, ArgAction, Command};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `--flag` / `--no-flag`; env accepts true/false/1/0/yes/no/on/off.
    Bool,
    /// `--opt <name>`
    Str,
    /// `--opt <name>` resolved as a filesystem path.
    Path,
    /// `--opt <n>` (u32).
    N32,
    /// `--opt <n>` (usize).
    Size,
    /// `--opt <n>` (f64).
    Float,
}

/// Where a resolved value came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    Flag,
    Env,
    Default,
}

pub struct OptDef {
    pub long: &'static str,
    pub env: &'static str,
    /// Program default. Empty string means "no default".
    pub default: &'static str,
    pub help: &'static str,
    pub kind: Kind,
    /// No default is acceptable: the run aborts if this is unset.
    pub required: bool,
}

impl OptDef {
    const fn s(
        long: &'static str,
        env: &'static str,
        default: &'static str,
        help: &'static str,
    ) -> Self {
        OptDef {
            long,
            env,
            default,
            help,
            kind: Kind::Str,
            required: false,
        }
    }
    const fn p(
        long: &'static str,
        env: &'static str,
        default: &'static str,
        help: &'static str,
    ) -> Self {
        OptDef {
            long,
            env,
            default,
            help,
            kind: Kind::Path,
            required: false,
        }
    }
    const fn b(long: &'static str, env: &'static str, default: bool, help: &'static str) -> Self {
        OptDef {
            long,
            env,
            default: if default { "true" } else { "false" },
            help,
            kind: Kind::Bool,
            required: false,
        }
    }
    const fn n(
        long: &'static str,
        env: &'static str,
        default: &'static str,
        help: &'static str,
    ) -> Self {
        OptDef {
            long,
            env,
            default,
            help,
            kind: Kind::N32,
            required: false,
        }
    }
    const fn z(
        long: &'static str,
        env: &'static str,
        default: &'static str,
        help: &'static str,
    ) -> Self {
        OptDef {
            long,
            env,
            default,
            help,
            kind: Kind::Size,
            required: false,
        }
    }
    const fn f(
        long: &'static str,
        env: &'static str,
        default: &'static str,
        help: &'static str,
    ) -> Self {
        OptDef {
            long,
            env,
            default,
            help,
            kind: Kind::Float,
            required: false,
        }
    }
    const fn req(long: &'static str, env: &'static str, help: &'static str) -> Self {
        OptDef {
            long,
            env,
            default: "",
            help,
            kind: Kind::Str,
            required: true,
        }
    }
}

pub const OPTIONS: &[OptDef] = &[
    // ---- paths -------------------------------------------------------------
    OptDef::p(
        "input-dir",
        "UNLOOT_INPUT_DIR",
        ".",
        "Directory scanned for loose photos (top level only; item folders below it are described, never rearranged)",
    ),
    OptDef::p(
        "output-dir",
        "UNLOOT_OUTPUT_DIR",
        ".",
        "Directory for item sub-directories and the CSV. Same as input-dir means work in place",
    ),
    // ---- LLM ---------------------------------------------------------------
    OptDef::req(
        "llm-url",
        "UNLOOT_LLM_URL",
        "OpenAI-compatible base URL, e.g. http://192.168.137.134:8010/v1 (no default, must be set)",
    ),
    OptDef::s(
        "llm-model",
        "UNLOOT_LLM_MODEL",
        "",
        "Model name. Empty asks the endpoint's /models for the one loaded model",
    ),
    OptDef::s(
        "llm-api-key",
        "UNLOOT_LLM_API_KEY",
        "",
        "Bearer token for the LLM endpoint. Empty sends no Authorization header",
    ),
    OptDef::n(
        "llm-timeout",
        "UNLOOT_LLM_TIMEOUT",
        "180",
        "Wall-clock timeout per LLM request, seconds",
    ),
    OptDef::n(
        "llm-retries",
        "UNLOOT_LLM_RETRIES",
        "4",
        "Attempts per LLM request. Retries cover network errors, HTTP 5xx, unparseable replies, and a \
         reasoning model that used all its token budget on thinking instead of answering — the budget is \
         doubled and then dropped on those retries, so one more attempt than you think you need is cheap \
         insurance",
    ),
    OptDef::n(
        "llm-image-edge",
        "UNLOOT_LLM_IMAGE_EDGE",
        "768",
        "Photos are sent to the LLM downscaled to this long edge, to keep prompts small (0 sends originals)",
    ),
    OptDef::b(
        "mock-llm",
        "UNLOOT_MOCK_LLM",
        false,
        "Offline mode: answer from a built-in guess, or from --mock-llm-file when given (no network at all)",
    ),
    OptDef::s(
        "mock-llm-file",
        "UNLOOT_MOCK_LLM_FILE",
        "",
        "With --mock-llm: JSON file of the form {\"calls\": [ ... ]}, replayed in call order: one boundary answer ({\"images\": [{\"id\", \"verdict\", \"subject\"}]}) then one description per item", // cspell:disable-line
    ),
    // ---- image output ------------------------------------------------------
    OptDef::n(
        "max-long-edge",
        "UNLOOT_MAX_LONG_EDGE",
        "1280",
        "Longest edge of output images, pixels (downscaled only, never upscaled)",
    ),
    OptDef::z(
        "max-bytes",
        "UNLOOT_MAX_BYTES",
        "307200",
        "Hard ceiling per output image, bytes (300 KiB)",
    ),
    OptDef::n(
        "jpeg-quality-min",
        "UNLOOT_JPEG_QUALITY_MIN",
        "62",
        "Lowest JPEG quality tried when hunting for max-bytes",
    ),
    OptDef::n(
        "jpeg-quality-max",
        "UNLOOT_JPEG_QUALITY_MAX",
        "92",
        "Highest JPEG quality tried when hunting for max-bytes",
    ),
    // ---- clustering --------------------------------------------------------
    OptDef::z(
        "window",
        "UNLOOT_WINDOW",
        "6",
        "Photos per LLM boundary-detection call",
    ),
    OptDef::z(
        "overlap",
        "UNLOOT_OVERLAP",
        "2",
        "Photos shared between consecutive windows so item boundaries survive window edges (must be < window)",
    ),
    OptDef::f(
        "gap-hint",
        "UNLOOT_GAP_HINT",
        "90",
        "Seconds of capture gap treated as weak evidence of a new item (hint only, the LLM decides)",
    ),
    // ---- safety ------------------------------------------------------------
    OptDef::b(
        "keep-source",
        "UNLOOT_KEEP_SOURCE",
        false,
        "Keep the full-resolution source photos after processing",
    ),
    OptDef::b(
        "resume",
        "UNLOOT_RESUME",
        true,
        "Skip photos already recorded in the state ledger",
    ),
    OptDef::b(
        "describe-dirs",
        "UNLOOT_DESCRIBE_DIRS",
        true,
        "Describe folders already sitting in the output directory and add the missing rows to the summary; their photos are read, never changed",
    ),
    OptDef::b(
        "dry-run",
        "UNLOOT_DRY_RUN",
        false,
        "Plan and print the CSV only; write and delete nothing",
    ),
    OptDef::b(
        "best-effort",
        "UNLOOT_BEST_EFFORT",
        false,
        "Continue when a window's boundary call fails, cutting that window at its largest capture gap",
    ),
    // ---- watch -------------------------------------------------------------
    OptDef::b(
        "watch",
        "UNLOOT_WATCH",
        false,
        "Keep running and process photos as they appear",
    ),
    OptDef::z(
        "watch-interval",
        "UNLOOT_WATCH_INTERVAL",
        "15",
        "Watch mode: seconds between scans of the input directory",
    ),
    OptDef::z(
        "settle",
        "UNLOOT_SETTLE",
        "20",
        "Watch mode: a photo must be this many seconds old (and unchanged) before it is picked up",
    ),
    // ---- naming & report ---------------------------------------------------
    OptDef::s(
        "name-template",
        "UNLOOT_NAME_TEMPLATE",
        "{category} {brand} {details} {color} size {size}",
        "Item directory name. Placeholders: {category} {brand} {details} {color} {size}. Empty fields drop their label word, so a belt with nothing else to say is 'Belt', not 'Belt size'",
    ),
    OptDef::s(
        "unbranded",
        "UNLOOT_UNBRANDED",
        "No brand",
        "Value written in the brand column when no brand is legible in the photos",
    ),
    OptDef::s(
        "na-label",
        "UNLOOT_NA_LABEL",
        "N/A",
        "Value written when a column does not apply to the item",
    ),
    OptDef::s(
        "currency",
        "UNLOOT_CURRENCY",
        "USD",
        "Price currency (USD prints weight in oz, anything else in g)",
    ),
    OptDef::s(
        "csv-name",
        "UNLOOT_CSV_NAME",
        "summary_{ts}.csv",
        "Report file name, relative to output-dir. {ts} is local time as yymmdd_hhmmss",
    ),
    OptDef::p(
        "state-file",
        "UNLOOT_STATE_FILE",
        ".unloot/state.json",
        "Resume ledger (relative paths resolve against output-dir)",
    ),
    OptDef::z(
        "jobs",
        "UNLOOT_JOBS",
        "0",
        "Parallel workers; 0 uses min(cpus, 4)",
    ),
    OptDef::s(
        "log",
        "UNLOOT_LOG",
        "info",
        "Log level: error, warn, info, debug, trace",
    ),
];

/// Look up an option definition by long name.
pub fn def(long: &str) -> &'static OptDef {
    OPTIONS
        .iter()
        .find(|o| o.long == long)
        .unwrap_or_else(|| panic!("unknown option {long}"))
}

/// Raw resolved value plus where it came from.
#[derive(Clone, Debug)]
struct Raw {
    value: String,
    origin: Origin,
}

/// Fully resolved, validated configuration.
#[derive(Debug)]
pub struct Config {
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
    pub llm_url: String,
    pub llm_model: String,
    pub llm_api_key: String,
    pub llm_timeout: u32,
    pub llm_retries: u32,
    pub llm_image_edge: u32,
    pub mock_llm: bool,
    pub mock_llm_file: String,
    pub max_long_edge: u32,
    pub max_bytes: usize,
    pub jpeg_quality_min: u32,
    pub jpeg_quality_max: u32,
    pub window: usize,
    pub overlap: usize,
    pub gap_hint: f64,
    pub keep_source: bool,
    pub resume: bool,
    /// Describe folders already in the output directory instead of ignoring them.
    pub describe_dirs: bool,
    pub dry_run: bool,
    pub best_effort: bool,
    pub watch: bool,
    pub watch_interval: usize,
    pub settle: usize,
    pub name_template: String,
    pub unbranded: String,
    pub na_label: String,
    pub currency: String,
    pub csv_name: String,
    pub state_file: PathBuf,
    pub jobs: usize,
    pub log: String,
}

fn arg_id(long: &str) -> String {
    long.replace('-', "_")
}

/// Build the CLI. Every option in [`OPTIONS`] becomes a flag; bools also get a
/// `--no-` negation so a false can be forced on the command line too.
fn command() -> Command {
    let mut cmd = Command::new("unloot")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Turn a folder of phone photos into per-item Vinted listing directories and a CSV summary.")
        .disable_help_subcommand(true)
        // Repeating an option is last-one-wins rather than an error: it happens
        // whenever a wrapper script appends to a flag the caller already gave.
        .args_override_self(true)
        .args([
            Arg::new("print_config")
                .long("print-config")
                .action(ArgAction::SetTrue)
                .help("Resolve every option (flag/env/default) and print the result with provenance, then exit"),
            Arg::new("print_options_md")
                .long("print-options-md")
                .action(ArgAction::SetTrue)
                .help("Print the options table as Markdown (used to keep README.md honest), then exit"),
        ]);

    for o in OPTIONS {
        let id = arg_id(o.long);
        let help = match o.kind {
            Kind::Bool => format!("{} (default: {}; env {})", o.help, o.default, o.env),
            _ if o.required => format!("{} (env {})", o.help, o.env),
            _ => format!("{} (default: {}; env {})", o.help, o.default, o.env),
        };
        let arg = Arg::new(id.clone()).long(o.long).help(&help);
        let arg = match o.kind {
            Kind::Bool => arg
                .action(ArgAction::SetTrue)
                .long_help(format!("{}. Use --no-{} to force off.", o.help, o.long))
                .overrides_with(arg_id(&format!("no-{}", o.long))),
            _ => arg
                .value_name(o.long.to_uppercase().replace('-', "_"))
                .action(ArgAction::Set),
        };
        cmd = cmd.arg(arg);

        if o.kind == Kind::Bool {
            cmd = cmd.arg(
                Arg::new(arg_id(&format!("no-{}", o.long)))
                    .long(format!("no-{}", o.long))
                    .action(ArgAction::SetTrue)
                    .hide(true),
            );
        }
    }
    cmd
}

fn parse_bool(s: &str) -> Result<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Ok(true),
        "0" | "false" | "no" | "n" | "off" | "" => Ok(false),
        other => bail!("expected a boolean, got {other:?}"),
    }
}

/// Resolve all options: explicit flag, then environment, then program default.
/// Resolve every option from the real command line and environment.
///
/// `--print-config` and `--print-options-md` short-circuit here, before the
/// required-option check, so you can ask what would happen on a machine that
/// is not yet configured.
pub fn resolve() -> Result<Config> {
    let args: Vec<OsString> = std::env::args_os().collect();
    let raw = raw_values(&args, &|k| std::env::var(k).ok())?;

    if args.iter().any(|a| a == "--print-options-md") {
        print!("{}", options_markdown());
        std::process::exit(0);
    }
    if args.iter().any(|a| a == "--print-config") {
        print_config(&raw);
        std::process::exit(0);
    }
    config_from_raw(&raw)
}

/// Same resolution, driven by explicit arguments and an environment lookup,
/// so the tests can exercise precedence without touching the real process
/// environment.
///
/// `args` is the argument list *without* a program name; clap wants argv[0],
/// so one is supplied here rather than by every caller.
#[cfg(test)]
pub fn resolve_from(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Result<Config> {
    let mut argv: Vec<OsString> = Vec::with_capacity(args.len() + 1);
    argv.push(OsString::from("unloot"));
    argv.extend(args.iter().map(OsString::from));
    let raw = raw_values(&argv, env)?;
    config_from_raw(&raw)
}

fn raw_values(
    args: &[OsString],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<BTreeMap<&'static str, Raw>> {
    let matches = command()
        .try_get_matches_from(args)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut raw: BTreeMap<&'static str, Raw> = BTreeMap::new();

    for o in OPTIONS {
        let mut chosen: Option<Raw> = None;

        if o.kind == Kind::Bool {
            let yes = matches.get_flag(&arg_id(o.long));
            let no = matches.get_flag(&arg_id(&format!("no-{}", o.long)));
            match (yes, no) {
                (true, false) => {
                    chosen = Some(Raw {
                        value: "true".into(),
                        origin: Origin::Flag,
                    })
                }
                (false, true) => {
                    chosen = Some(Raw {
                        value: "false".into(),
                        origin: Origin::Flag,
                    })
                }
                (true, true) => bail!("--{} and --no-{} cannot be combined", o.long, o.long),
                _ => {}
            }
        } else if matches.value_source(&arg_id(o.long))
            == Some(clap::parser::ValueSource::CommandLine)
        {
            chosen = Some(Raw {
                value: matches
                    .get_one::<String>(&arg_id(o.long))
                    .cloned()
                    .unwrap_or_default(),
                origin: Origin::Flag,
            });
        }

        if chosen.is_none()
            && let Some(v) = env(o.env)
        {
            // An env var set to an empty string counts as unset for options
            // that already have a sensible default; --llm-model "" is the
            // deliberate "ask the endpoint" case and stays meaningful.
            if !v.trim().is_empty() || o.kind == Kind::Bool || o.default.is_empty() {
                chosen = Some(Raw {
                    value: v,
                    origin: Origin::Env,
                });
            }
        }

        let chosen = chosen.or_else(|| {
            (!o.default.is_empty() || o.kind == Kind::Bool).then(|| Raw {
                value: o.default.to_string(),
                origin: Origin::Default,
            })
        });

        if let Some(v) = chosen {
            raw.insert(o.long, v);
        }
    }
    Ok(raw)
}

fn config_from_raw(raw: &BTreeMap<&'static str, Raw>) -> Result<Config> {
    for o in OPTIONS {
        if o.required && !raw.contains_key(o.long) {
            bail!(
                "--{} is required: set it with --{} <value> or env {}",
                o.long,
                o.long,
                o.env
            );
        }
    }

    let get = |k: &str| raw.get(k).map(|r| r.value.clone()).unwrap_or_default();
    let get_bool =
        |k: &str| parse_bool(&get(k)).with_context(|| format!("invalid value for {}", def(k).env));
    let get_u32 = |k: &str| -> Result<u32> {
        get(k)
            .trim()
            .parse()
            .with_context(|| format!("invalid integer for {}", def(k).env))
    };
    let get_usize = |k: &str| -> Result<usize> {
        get(k)
            .trim()
            .parse()
            .with_context(|| format!("invalid integer for {}", def(k).env))
    };
    let get_f64 = |k: &str| -> Result<f64> {
        get(k)
            .trim()
            .parse()
            .with_context(|| format!("invalid number for {}", def(k).env))
    };

    let input_dir = PathBuf::from(get("input-dir"));
    let output_dir = PathBuf::from(get("output-dir"));
    let state_file = {
        let p = PathBuf::from(get("state-file"));
        if p.is_absolute() {
            p
        } else {
            output_dir.join(p)
        }
    };

    let mut cfg = Config {
        input_dir,
        output_dir,
        llm_url: get("llm-url").trim().trim_end_matches('/').to_string(),
        llm_model: get("llm-model").trim().to_string(),
        llm_api_key: get("llm-api-key").trim().to_string(),
        llm_timeout: get_u32("llm-timeout")?,
        llm_retries: get_u32("llm-retries")?,
        llm_image_edge: get_u32("llm-image-edge")?,
        mock_llm: get_bool("mock-llm")?,
        mock_llm_file: get("mock-llm-file"),
        max_long_edge: get_u32("max-long-edge")?,
        max_bytes: get_usize("max-bytes")?,
        jpeg_quality_min: get_u32("jpeg-quality-min")?,
        jpeg_quality_max: get_u32("jpeg-quality-max")?,
        window: get_usize("window")?,
        overlap: get_usize("overlap")?,
        gap_hint: get_f64("gap-hint")?,
        keep_source: get_bool("keep-source")?,
        resume: get_bool("resume")?,
        describe_dirs: get_bool("describe-dirs")?,
        dry_run: get_bool("dry-run")?,
        best_effort: get_bool("best-effort")?,
        watch: get_bool("watch")?,
        watch_interval: get_usize("watch-interval")?,
        settle: get_usize("settle")?,
        name_template: get("name-template"),
        unbranded: get("unbranded"),
        na_label: get("na-label"),
        currency: get("currency"),
        csv_name: get("csv-name"),
        state_file,
        jobs: get_usize("jobs")?,
        log: get("log"),
    };

    cfg.validate()?;
    Ok(cfg)
}

impl Config {
    fn validate(&mut self) -> Result<()> {
        if self.window < 2 {
            bail!("--window must be at least 2 (one photo to compare against the next)");
        }
        if self.overlap >= self.window {
            bail!(
                "--overlap ({}) must be smaller than --window ({})",
                self.overlap,
                self.window
            );
        }
        if self.max_long_edge < 64 {
            bail!("--max-long-edge must be at least 64");
        }
        if self.max_bytes < 16 * 1024 {
            bail!("--max-bytes must be at least 16384");
        }
        let (lo, hi) = (self.jpeg_quality_min, self.jpeg_quality_max);
        if !(1..=100).contains(&lo) || !(1..=100).contains(&hi) || lo > hi {
            bail!("JPEG quality range invalid: min {lo}, max {hi} (need 1 <= min <= max <= 100)");
        }
        if !self.mock_llm && self.llm_url.is_empty() {
            bail!(
                "no LLM endpoint: set --llm-url or UNLOOT_LLM_URL (or --mock-llm to run offline)"
            );
        }
        if self.currency.trim().is_empty() {
            bail!("--currency cannot be empty");
        }
        if self.jobs == 0 {
            self.jobs = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(4);
        }
        if self.watch_interval == 0 {
            bail!("--watch-interval must be at least 1");
        }
        if !self.csv_name.contains("{ts}") {
            bail!("--csv-name must contain {{ts}} so reports never overwrite each other");
        }
        Ok(())
    }

    /// True when input and output are the same tree.
    /// True when the listing should weigh in ounces — the US convention, and
    /// the one Vinted's American buyers read prices in.
    pub fn uses_ounces(&self) -> bool {
        self.currency.trim().eq_ignore_ascii_case("USD")
    }

    /// The weight column's header: ounces for a US listing, grams elsewhere.
    pub fn weight_column(&self) -> String {
        if self.uses_ounces() {
            "weight (oz)".into()
        } else {
            "weight (g)".into()
        }
    }

    pub fn in_place(&self) -> bool {
        fn canon(p: &PathBuf) -> PathBuf {
            std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())
        }
        canon(&self.input_dir) == canon(&self.output_dir)
    }
}

fn print_config(raw: &BTreeMap<&'static str, Raw>) {
    let width = raw.keys().map(|k| k.len()).max().unwrap_or(0);
    for (k, v) in raw {
        let o = def(k);
        let shown = if o.env.contains("API_KEY") && !v.value.is_empty() {
            "<set, redacted>"
        } else {
            v.value.as_str()
        };
        println!("{k:<width$} = {shown:<24} ({:?})", v.origin);
    }
}

/// Markdown table of every option, for README.md.
pub fn options_markdown() -> String {
    let mut out = String::new();
    out.push_str("| Flag | Environment | Default | Meaning |\n");
    out.push_str("| --- | --- | --- | --- |\n");
    for o in OPTIONS {
        let default = if o.required {
            "**required, no default**".to_string()
        } else {
            format!("`{}`", o.default)
        };
        let flag = if o.kind == Kind::Bool {
            format!("`--{}` / `--no-{}`", o.long, o.long)
        } else {
            format!("`--{}`", o.long)
        };
        out.push_str(&format!(
            "| {flag} | `{}` | {default} | {} |\n",
            o.env, o.help
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_option_has_env_prefixed_and_unique_names() {
        let mut seen = std::collections::HashSet::new();
        for o in OPTIONS {
            assert!(o.env.starts_with("UNLOOT_"), "{} env not prefixed", o.env);
            assert!(seen.insert(o.long), "duplicate option {}", o.long);
            assert!(seen.insert(o.env), "duplicate env {}", o.env);
            assert!(!o.help.is_empty(), "{} has no help", o.long);
            assert!(
                !o.default.contains("UNLOOT_"),
                "{} default is not a default",
                o.long
            );
        }
    }

    #[test]
    fn bool_env_parsing() {
        assert!(parse_bool("yes").unwrap());
        assert!(parse_bool("TRUE").unwrap());
        assert!(!parse_bool("off").unwrap());
        assert!(!parse_bool("").unwrap());
        assert!(parse_bool("maybe").is_err());
    }

    #[test]
    fn markdown_table_has_a_row_per_option() {
        let md = options_markdown();
        assert_eq!(
            md.lines().filter(|l| l.starts_with("| `")).count(),
            OPTIONS.len()
        );
    }

    /// The env lookup the tests use instead of the process environment.
    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| owned.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.clone())
    }

    fn resolve(args: &[&str], env: &[(&str, &str)]) -> Config {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let lookup = env_from(env);
        // Every test has to name an endpoint; it is the one compulsory option.
        let mut with_url = owned.clone();
        if !args.iter().any(|a| a.starts_with("--llm-url"))
            && !env.iter().any(|(k, _)| *k == "UNLOOT_LLM_URL")
        {
            with_url.extend(["--llm-url".to_string(), "http://example/v1".to_string()]);
        }
        resolve_from(&with_url, &lookup).unwrap()
    }

    #[test]
    fn defaults_apply_when_nothing_else_is_set() {
        let cfg = resolve(&[], &[]);
        assert_eq!(cfg.max_long_edge, 1280);
        assert_eq!(cfg.max_bytes, 307_200);
        assert_eq!(cfg.currency, "USD");
        assert!(
            !cfg.keep_source,
            "deleting sources is the default and must stay so"
        );
        assert!(cfg.resume);
        assert_eq!(cfg.window, 6);
        assert_eq!(cfg.overlap, 2);
    }

    #[test]
    fn env_overrides_the_program_default() {
        let cfg = resolve(
            &[],
            &[("UNLOOT_MAX_LONG_EDGE", "800"), ("UNLOOT_CURRENCY", "EUR")],
        );
        assert_eq!(cfg.max_long_edge, 800);
        assert_eq!(cfg.currency, "EUR");
    }

    #[test]
    fn flag_beats_env_beats_default() {
        // The whole precedence rule in one test: the same option set three ways.
        let env_only = resolve(&[], &[("UNLOOT_MAX_BYTES", "100000")]);
        assert_eq!(env_only.max_bytes, 100_000);
        let both = resolve(&["--max-bytes", "65536"], &[("UNLOOT_MAX_BYTES", "100000")]);
        assert_eq!(
            both.max_bytes, 65_536,
            "a flag must win over the environment"
        );
    }

    #[test]
    fn a_false_can_be_forced_with_no_dash_even_when_the_env_says_yes() {
        let on = resolve(&[], &[("UNLOOT_KEEP_SOURCE", "yes")]);
        assert!(on.keep_source);
        let off = resolve(&["--no-keep-source"], &[("UNLOOT_KEEP_SOURCE", "yes")]);
        assert!(
            !off.keep_source,
            "--no-keep-source must beat UNLOOT_KEEP_SOURCE=yes"
        );
    }

    #[test]
    fn the_endpoint_is_the_one_thing_you_cannot_leave_out() {
        let owned = vec!["--dry-run".to_string()];
        let none = env_from(&[]);
        let e = resolve_from(&owned, &none).unwrap_err().to_string();
        assert!(e.contains("llm-url"), "{e}");
        let by_env = env_from(&[("UNLOOT_LLM_URL", "http://box:8010/v1")]);
        assert_eq!(
            resolve_from(&owned, &by_env).unwrap().llm_url,
            "http://box:8010/v1"
        );
    }

    #[test]
    fn nonsense_is_refused_before_any_photo_is_touched() {
        // Each of these would silently produce rubbish: overlap as big as the
        // window means no photo is ever compared with its predecessor.
        for (flag, value) in [
            ("--overlap", "6"),
            ("--max-long-edge", "32"),
            ("--max-bytes", "1024"),
            ("--jpeg-quality-min", "95"),
            ("--watch-interval", "0"),
        ] {
            let args = vec![flag.to_string(), value.to_string()];
            let lookup = env_from(&[]);
            let mut full = vec!["--llm-url".to_string(), "http://x/v1".to_string()];
            full.extend(args);
            assert!(
                resolve_from(&full, &lookup).is_err(),
                "{flag} {value} should not resolve"
            );
        }
    }

    #[test]
    fn the_csv_name_must_carry_a_timestamp() {
        // Without {ts} every run would overwrite yesterday's listing.
        let lookup = env_from(&[
            ("UNLOOT_LLM_URL", "http://x/v1"),
            ("UNLOOT_CSV_NAME", "summary.csv"),
        ]);
        let owned = vec!["--input-dir".to_string(), ".".to_string()];
        assert!(resolve_from(&owned, &lookup).is_err());
    }

    #[test]
    fn weight_column_follows_the_currency() {
        assert_eq!(resolve(&[], &[]).weight_column(), "weight (oz)");
        assert_eq!(
            resolve(&[], &[("UNLOOT_CURRENCY", "EUR")]).weight_column(),
            "weight (g)"
        );
    }
}
