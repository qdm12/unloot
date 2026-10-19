//! Where the model does the thinking: which photos belong to one item, and
//! what that item is.
//!
//! Two passes, deliberately. Pass 1 walks the photos in overlapping windows
//! and asks only "same item or next item?" — a small question the model answers
//! well, and one whose answer we can validate (one verdict per photo, in
//! order). Pass 2 catalogues each finished cluster. Merging both into a single
//! call is tempting and worse: a model asked to name an item while deciding
//! where the item ends will bend the boundary to fit the name.
//!
//! Capture gaps are passed to the model as a labelled hint and nothing more.
//! Photographing the next item two seconds after the last is normal, so a gap
//! is evidence, not a rule; the pixels decide.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::llm::{self, Llm, Part};
use crate::scan::SourceImage;

/// Categories the model is nudged towards, so names and columns stay
/// consistent across runs. Free text is still accepted.
const CATEGORIES: &str = "T-shirt, Shirt, Blouse, Sweater, Hoodie, Jacket, Coat, Gilet, Trousers, Jeans, Shorts, Skirt, Dress, Bodysuit, Leggings, Swimwear, Underwear, Socks, Shoes, Sandals, Boots, Trainers, Hat, Scarf, Gloves, Belt, Bag, Rucksack, Wallet, Keys, Jewellery, Watch, Sunglasses, Phone case, Charger, Cable, Power bank, Headphones, Speaker, Games, Console, Book, Dog toy, Pet toy, Toy, Homeware, Other";

/// Photos sent to the model when cataloguing one item (evenly sampled above
/// this). Past a handful the extra angles stop adding information.
const MAX_ANALYSIS_IMAGES: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Boundary {
    /// Same item as the photo before it.
    Same,
    /// First photo of a new item.
    New,
}

#[derive(Clone, Debug)]
pub struct Verdict {
    pub boundary: Boundary,
    /// What the model thinks is in the photo — used to cross-check its own call.
    pub subject: String,
}

#[derive(Clone, Debug)]
pub struct Cluster {
    pub images: Vec<SourceImage>,
    /// True when the boundary of the first photo was guessed rather than seen.
    pub fallback: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Meta {
    pub category: String,
    pub brand: Option<String>,
    pub color: String,
    pub size: Option<String>,
    /// What distinguishes this thing from every other item of the same
    /// category: material, pattern, a feature, what it is for. Feeds the
    /// directory name, which is otherwise hopelessly vague — "Toy orange" tells
    /// nobody anything, "Dog toy white and orange rope knot" does.
    pub details: String,
    pub price: f64,
    pub weight_g: f64,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub name: String,
    pub meta: Meta,
    pub images: Vec<SourceImage>,
}

// ---------------------------------------------------------------------------
// Pass 1 — boundaries
// ---------------------------------------------------------------------------

const BOUNDARY_SYSTEM: &str = "You look at photographs of personal items being photographed for sale second-hand. \
The photos arrive in capture order and several consecutive photos usually show the SAME item from different angles. \
For each photo decide whether it shows the same item as the photo immediately before it, or the first photo of a new item. \
Judge on the pixels: the object itself, its shape, colour, material, pattern, any logo, and how it is held or laid out. \
A change of angle, zoom, framing or background is the SAME item. A different object is a NEW item, even if the \
background and the hand holding it are identical. A stated time gap is a weak hint only: two items photographed \
seconds apart are still two items, and one item photographed minutes apart is still one item. \
You must answer with a single JSON object and nothing else.";

/// Build the window's request. `previous` is the photo immediately before the
/// window in capture order: without it the first photo of a window would look
/// like the start of the folder and be biased towards `new`.
fn boundary_prompt(
    cfg: &Config,
    window: &[&SourceImage],
    previous: Option<&SourceImage>,
) -> Vec<Part> {
    let mut parts = Vec::with_capacity(window.len() * 2 + 2);
    let mut header = String::from("These photos are in capture order.\n");
    for (k, img) in window.iter().enumerate() {
        let before = if k == 0 {
            previous
        } else {
            Some(window[k - 1])
        };
        let gap = img.gap_from(before);
        let gap_text = match gap {
            Some(g) => format!(" gap={g:.0}s"),
            None => String::new(),
        };
        header.push_str(&format!("id={} file={}{}\n", img.id, img.name, gap_text));
    }
    header.push_str(&format!(
        "\nTreat a gap of about {:.0}s or more as weak evidence of a new item, and a small gap as no evidence either way. \
The first photo shown may continue an item you have already seen, so judge it like the others; if you cannot tell, answer \"new\".\n\
Reply with JSON exactly like this, one entry per photo, same order, no other keys:\n\
{{\"images\":[{{\"id\":1,\"verdict\":\"new\",\"subject\":\"black leather bifold wallet\"}},\
{{\"id\":2,\"verdict\":\"same\",\"subject\":\"black leather bifold wallet, open\"}}]}}\n",
        cfg.gap_hint
    ));
    parts.push(Part::Text(header));

    for img in window {
        parts.push(Part::Text(format!(
            "Photo id={} of file {}",
            img.id, img.name
        )));
        match llm::prepare_llm_image(&img.path, cfg.llm_image_edge) {
            Ok(bytes) => parts.push(Part::Image(img.name.clone(), bytes)),
            Err(e) => {
                warn!(path = %img.path.display(), error = %format!("{e:#}"), "sending this photo to the model failed; describing its absence");
                parts.push(Part::Text("[photo could not be read]".into()));
            }
        }
    }
    parts.push(Part::Text(
        "Now reply with the JSON object only: one entry per photo, in the order given.".into(),
    ));
    parts
}

fn parse_verdicts(v: &Value, window: &[&SourceImage]) -> Result<Vec<Verdict>> {
    let arr = v
        .get("images")
        .and_then(|a| a.as_array())
        .ok_or_else(|| anyhow!("reply has no \"images\" array"))?;
    if arr.len() != window.len() {
        bail!(
            "reply listed {} photos, window had {}",
            arr.len(),
            window.len()
        );
    }
    let mut out = Vec::with_capacity(arr.len());
    for (k, entry) in arr.iter().enumerate() {
        let expect = window[k].id;
        let got_id = entry
            .get("id")
            .map(|x| match x {
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.trim().to_string(),
                _ => String::new(),
            })
            .unwrap_or_default();
        if got_id != expect.to_string() {
            bail!(
                "reply entry {} is for id {got_id:?}, expected {expect}",
                k + 1
            );
        }
        let raw = entry
            .get("verdict")
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_ascii_lowercase())
            .unwrap_or_default();
        let boundary = match raw.as_str() {
            "same" | "s" | "continue" | "continues" => Boundary::Same,
            "new" | "n" | "different" | "next" => Boundary::New,
            other => bail!("unreadable verdict {other:?} for photo {expect}"),
        };
        out.push(Verdict {
            boundary,
            subject: entry
                .get("subject")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim()
                .to_string(),
        });
    }
    Ok(out)
}

/// Verdicts for one window, retrying until the model answers in full.
fn verdicts_for_window(
    cfg: &Config,
    llm_client: &dyn Llm,
    window: &[&SourceImage],
    previous: Option<&SourceImage>,
    what: &str,
) -> Result<Vec<Verdict>> {
    let req = llm::Request {
        system: BOUNDARY_SYSTEM,
        parts: boundary_prompt(cfg, window, previous),
        max_tokens: 512 + 96 * window.len() as u32,
        temperature: 0.0,
        what: what.to_string(),
    };
    let reply = llm_client.complete_json(&req)?;
    let verdicts = parse_verdicts(&reply, window).with_context(|| {
        format!(
            "model answered the boundary question for photo {} in a way I could not use",
            window[0].id
        )
    })?;

    // Cross-check: a subject that clearly changed while the verdict says
    // "same" is worth a line in the log, but the verdict stands.
    for k in 1..verdicts.len() {
        let (a, b) = (&verdicts[k - 1].subject, &verdicts[k].subject);
        if verdicts[k].boundary == Boundary::Same && !a.is_empty() && !b.is_empty() && a != b {
            debug!(
                photo = window[k].id,
                previous = %a,
                current = %b,
                "verdict says same but the described subject changed"
            );
        }
    }
    Ok(verdicts)
}

/// Last-resort verdicts: cut the window at its largest capture gap if that gap
/// is itself suspicious, otherwise keep the window as one item.
fn fallback_verdicts(window: &[&SourceImage], gap_hint: f64) -> Vec<Verdict> {
    let mut best: Option<(usize, f64)> = None;
    for k in 1..window.len() {
        if let Some(g) = window[k].gap_from(Some(window[k - 1]))
            && best.map(|(_, bg)| g > bg).unwrap_or(true)
        {
            best = Some((k, g));
        }
    }
    let cut = match best {
        Some((k, g)) if g >= gap_hint => Some(k),
        _ => None,
    };
    (0..window.len())
        .map(|k| Verdict {
            boundary: if k == 0 || Some(k) == cut {
                Boundary::New
            } else {
                Boundary::Same
            },
            subject: String::new(),
        })
        .collect()
}

/// Group capture-ordered photos into one cluster per item.
pub fn cluster(cfg: &Config, llm_client: &dyn Llm, images: &[SourceImage]) -> Result<Vec<Cluster>> {
    if images.is_empty() {
        return Ok(Vec::new());
    }
    if images.len() == 1 {
        return Ok(vec![Cluster {
            images: images.to_vec(),
            fallback: false,
        }]);
    }

    let window = cfg.window;
    let step = window - cfg.overlap;
    let refs: Vec<&SourceImage> = images.iter().collect();

    // Global index -> verdict. First window to speak for a photo wins, because
    // it is the window that saw that photo together with its predecessor.
    let mut verdicts: BTreeMap<usize, Verdict> = BTreeMap::new();
    let mut failures = 0usize;
    // Photo indexes whose "new item" came from the capture-gap fallback rather
    // than from the model, so the item can be flagged as a guessed boundary.
    let mut guessed_starts: std::collections::HashSet<usize> = std::collections::HashSet::new();

    let mut start = 0usize;
    let mut win_no = 0usize;
    while start < images.len() {
        let end = (start + window).min(images.len());
        let slice = &refs[start..end];
        win_no += 1;
        let what = format!("boundary {win_no} (photos {}-{end})", images[start].id);
        let previous: Option<&SourceImage> = if start == 0 {
            None
        } else {
            Some(refs[start - 1])
        };
        match verdicts_for_window(cfg, llm_client, slice, previous, &what) {
            Ok(vs) => {
                for (k, v) in vs.into_iter().enumerate() {
                    verdicts.entry(start + k).or_insert(v);
                }
            }
            Err(e) => {
                failures += 1;
                if !cfg.best_effort {
                    return Err(e).with_context(|| {
                        format!(
                            "could not decide item boundaries for photos {}-{end}",
                            images[start].id
                        )
                    });
                }
                warn!(error = %format!("{e:#}"), photos = %format!("{}-{end}", images[start].id), "falling back to a capture-gap cut for this window");
                let fb = fallback_verdicts(slice, cfg.gap_hint);
                for (k, v) in fb.into_iter().enumerate() {
                    if v.boundary == Boundary::New && start + k != 0 {
                        guessed_starts.insert(start + k);
                    }
                    verdicts.entry(start + k).or_insert(v);
                }
            }
        }
        if end == images.len() {
            break;
        }
        start += step;
    }

    if failures > 0 {
        warn!(failures, "some boundary decisions were guesses");
    }

    let mut clusters: Vec<Cluster> = Vec::new();
    for (i, img) in images.iter().enumerate() {
        let v = verdicts.get(&i).cloned().unwrap_or(Verdict {
            boundary: Boundary::New,
            subject: String::new(),
        });
        if i == 0 || v.boundary == Boundary::New || clusters.is_empty() {
            let guessed = guessed_starts.contains(&i);
            clusters.push(Cluster {
                images: vec![img.clone()],
                fallback: guessed,
            });
        } else {
            clusters
                .last_mut()
                .expect("cluster exists")
                .images
                .push(img.clone());
        }
    }
    let guessed = clusters.iter().filter(|c| c.fallback).count();
    info!(
        items = clusters.len(),
        photos = images.len(),
        guessed_boundaries = guessed,
        "item boundaries settled"
    );
    if guessed > 0 {
        warn!(
            guessed,
            "some items were split on a capture-gap guess because the model could not decide; check those folders"
        );
    }
    Ok(clusters)
}

// ---------------------------------------------------------------------------
// Pass 2 — catalogue each item
// ---------------------------------------------------------------------------

fn analysis_system(cfg: &Config) -> String {
    format!(
        "You catalogue a single item for a second-hand listing. Look at the photos of the item and report facts you \
can support from the pixels; when you cannot see something, answer null rather than inventing it. \
A size or brand printed on a visible tag is much better evidence than a guess from how big it looks. \
Category must be one of: {CATEGORIES}. Pick the most specific one that fits — \
\"Dog toy\" rather than \"Toy\", \"Charger\" rather than \"Electronics\"; if truly none fits, use a short \
common noun in English. \
Colour is the dominant colour of the item itself, not of the background, in English lowercase; if it is \
two colours, say both, as in \"white and orange\". \
Brand ONLY if a brand name is actually legible in the photos — printed on a tag, embossed, on packaging, \
on the item itself. A logo you think you recognise is not enough; if you cannot read the letters, answer \
null. Most things have no visible brand and null is the common answer. \
Size from a visible tag only, null otherwise. \
Details is two to six words that tell a buyer which one this is: material, pattern, what it is for, or a \
feature — \"leather with red star\", \"USB type-A and type-C\", \"rope knot for dog chewing\". Text printed \
on the item or its box may be quoted here in short fragments, but only what you can actually read, and \
never who made or owned it, where it came from, or what condition it is in. Lowercase, no brand, no \
colour (they have their own fields), and null if nothing distinguishes it. \
Price is what this individual used item in the condition shown would roughly sell for in {cur}, as a number; \
be realistic, second-hand prices are low. Weight is the item's own weight in grams, a wild guess is fine. \
Answer with a single JSON object and nothing else, keys exactly: \
{{\"category\":\"Wallet\",\"brand\":null,\"color\":\"black\",\"size\":null,\"details\":\"leather with red star\",\"price\":12,\"weight_g\":220}}",
        cur = cfg.currency
    )
}

fn analysis_parts(cfg: &Config, cluster: &Cluster) -> Vec<Part> {
    let chosen = sample(&cluster.images, MAX_ANALYSIS_IMAGES);
    let mut parts = vec![Part::Text(format!(
        "Describe the item in these {} photo(s) — all of them are the same item. Reply with the JSON object only.\n",
        chosen.len()
    ))];
    for img in chosen {
        parts.push(Part::Text(format!("Photo from {}", img.name)));
        match llm::prepare_llm_image(&img.path, cfg.llm_image_edge) {
            Ok(bytes) => parts.push(Part::Image(img.name.clone(), bytes)),
            Err(e) => {
                warn!(path = %img.path.display(), error = %format!("{e:#}"), "skipping photo for analysis")
            }
        }
    }
    parts
}

/// Evenly spaced subset, always keeping the first photo.
fn sample(images: &[SourceImage], max: usize) -> Vec<&SourceImage> {
    if images.len() <= max {
        return images.iter().collect();
    }
    if max == 0 {
        return Vec::new();
    }
    if max == 1 {
        return vec![&images[0]];
    }
    let mut out = Vec::with_capacity(max);
    // Spread the picks over the whole run, so the last photo is in there too:
    // the closing angle is often the one that settles what the item is.
    for i in 0..max {
        let idx = (i as f64 * (images.len() - 1) as f64 / (max - 1) as f64).round() as usize;
        let img = &images[idx.min(images.len() - 1)];
        if out.last().map(|p: &&SourceImage| p.id).ne(&Some(img.id)) {
            out.push(img);
        }
    }
    out
}

fn parse_meta(v: &Value, cfg: &Config) -> Result<Meta> {
    let text = |keys: &[&str]| -> Option<String> {
        keys.iter().filter_map(|k| v.get(*k)).find_map(|x| match x {
            Value::String(s) => {
                let t = s.trim();
                (!t.is_empty() && !t.eq_ignore_ascii_case("null") && !t.eq_ignore_ascii_case("n/a"))
                    .then(|| t.to_string())
            }
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
    };
    let number = |keys: &[&str]| -> Option<f64> {
        keys.iter().filter_map(|k| v.get(*k)).find_map(|x| match x {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s
                .chars()
                .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
                .collect::<String>()
                .parse()
                .ok(),
            _ => None,
        })
    };

    let category =
        text(&["category", "category_name"]).ok_or_else(|| anyhow!("reply has no category"))?;
    let brand = text(&["brand", "brand_name"]).or_else(|| text(&["brand_name_guess"]));
    let color = text(&["color", "colour"]).unwrap_or_else(|| "unknown".into());
    let size = text(&["size", "size_tag"]).or_else(|| text(&["size_label"]));
    // Absent or null details are fine: the name template drops an empty field.
    let details = text(&["details", "description", "features"]).unwrap_or_default();

    let price = number(&[
        "price",
        &format!("price_{}", cfg.currency.to_lowercase()),
        "price_usd",
        "price_eur",
    ])
    .ok_or_else(|| anyhow!("reply has no price"))?;
    let weight_g = number(&["weight_g", "weight", "weight_grams"]).unwrap_or(0.0);

    if price < 0.0 {
        bail!("negative price {price}");
    }
    Ok(Meta {
        category: clean_word(&category),
        brand: brand.map(|b| brand_case(&clean_word(&b))),
        color: color.to_lowercase(),
        size,
        details: clean_word(&details),
        price,
        weight_g,
    })
}

/// The model returns brands in whatever case it happens to feel like — "pupjoy",
/// "enki". Capitalising the first letter of each word makes them read like names
/// in a folder and a spreadsheet, and leaves anything already capitalised alone.
fn brand_case(s: &str) -> String {
    s.split_whitespace()
        .map(|word| match word.chars().next() {
            Some(first) if first.is_lowercase() => {
                let mut rest = word.chars();
                rest.next();
                first.to_uppercase().collect::<String>() + rest.as_str()
            }
            _ => word.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn clean_word(s: &str) -> String {
    let mut out: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    out = out.trim_matches(|c| c == '.' || c == ',').to_string();
    if out.chars().count() > 60 {
        out = out.chars().take(60).collect();
    }
    out
}

/// Catalogue every cluster. One call per item; items are independent, so they
/// go out in parallel when `--jobs` allows it.
/// One model call: what is in these photos?
fn describe_cluster(
    cfg: &Config,
    llm_client: &dyn Llm,
    label: &str,
    cluster: &Cluster,
) -> Result<Meta> {
    let req = llm::Request {
        system: &analysis_system(cfg),
        parts: analysis_parts(cfg, cluster),
        max_tokens: 640,
        temperature: 0.0,
        what: label.to_string(),
    };
    let reply = llm_client
        .complete_json(&req)
        .with_context(|| format!("analysis of {label}"))?;
    parse_meta(&reply, cfg).with_context(|| format!("analysis of {label} was unusable"))
}

/// Describe folders that were sorted before unloot saw them, one item per
/// folder. The caller keeps the folder's own name — only the attributes come
/// from the model — and nothing inside is written to.
pub fn describe_folders(
    cfg: &Config,
    llm_client: &dyn Llm,
    dirs: Vec<crate::scan::ItemDir>,
) -> Vec<Result<(crate::scan::ItemDir, Meta)>> {
    each_item(cfg, dirs, &|_n, dir| {
        let label = format!(
            "folder {} ({} photos, first {})",
            dir.name,
            dir.images.len(),
            dir.images[0].name
        );
        let cluster = Cluster {
            images: dir.images.clone(),
            fallback: false,
        };
        describe_cluster(cfg, llm_client, &label, &cluster).map(|meta| (dir, meta))
    })
}

/// One model call per item, in a pool when asked to. Order is preserved either
/// way, and a pool that will not start is not worth failing a run over.
fn each_item<T: Send, R: Send>(
    cfg: &Config,
    items: Vec<T>,
    f: &(dyn Fn(usize, T) -> Result<R> + Sync),
) -> Vec<Result<R>> {
    use rayon::prelude::*;
    if cfg.jobs <= 1 {
        return items
            .into_iter()
            .enumerate()
            .map(|(n, x)| f(n, x))
            .collect();
    }
    match rayon::ThreadPoolBuilder::new()
        .num_threads(cfg.jobs)
        .build()
    {
        Ok(pool) => pool.install(|| {
            items
                .into_par_iter()
                .enumerate()
                .map(|(n, x)| f(n, x))
                .collect()
        }),
        Err(e) => {
            warn!(error = %e, "could not start the analysis thread pool; running one at a time");
            items
                .into_iter()
                .enumerate()
                .map(|(n, x)| f(n, x))
                .collect()
        }
    }
}

pub fn catalogue(cfg: &Config, llm_client: &dyn Llm, clusters: Vec<Cluster>) -> Result<Vec<Item>> {
    let results: Vec<Result<Item>> = each_item(cfg, clusters, &|n, c: Cluster| {
        let label = format!(
            "item {} ({} photos, first {})",
            n + 1,
            c.images.len(),
            c.images[0].name
        );
        let meta = describe_cluster(cfg, llm_client, &label, &c)?;
        let name = render_name(cfg, &meta).with_context(|| format!("naming {label}"))?;
        Ok(Item {
            name,
            meta,
            images: c.images,
        })
    });

    let mut items = Vec::new();
    let mut failed = 0;
    for (i, r) in results.into_iter().enumerate() {
        match r {
            Ok(item) => items.push(item),
            Err(e) if cfg.best_effort => {
                failed += 1;
                warn!(error = %format!("{e:#}"), item = i + 1, "could not catalogue this item; naming it from its first photo");
                items.push(Item {
                    name: format!("Item {}", i + 1),
                    meta: Meta::default(),
                    images: Vec::new(),
                });
            }
            Err(e) => return Err(e),
        }
    }
    if failed > 0 {
        warn!(failed, "items catalogued from a fallback");
    }
    Ok(items)
}

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

/// Words that only make sense next to a value; dropped with an empty field.
const LABEL_WORDS: &[&str] = &["size", "color", "colour", "brand", "in", "de", "en", "of"];

/// Render `--name-template` into a directory name.
///
/// Empty fields disappear together with the label word in front of them, so the
/// default `"{category} {color} size {size}"` gives `Skirt blue size S` for a
/// skirt and `Wallet black` for a wallet — never `Wallet black size`.
pub fn render_name(cfg: &Config, meta: &Meta) -> Result<String> {
    let fields = [
        ("category", meta.category.clone()),
        ("color", meta.color.clone()),
        ("size", meta.size.clone().unwrap_or_default()),
        ("details", meta.details.clone()),
        ("brand", meta.brand.clone().unwrap_or_default()),
    ];
    let mut out = String::new();
    let tmpl = &cfg.name_template;
    let mut rest = tmpl.as_str();
    while let Some(open) = rest.find('{') {
        let (before, tail) = rest.split_at(open);
        let close = tail
            .find('}')
            .ok_or_else(|| anyhow!("--name-template has an unterminated placeholder: {tail}"))?;
        let key = &tail[1..close];
        let value = fields
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| anyhow!("unknown placeholder {key:?} in --name-template (use category, color, details, brand, size)"))?;
        out.push_str(before);
        if value.is_empty() {
            // Drop the label word that was introducing this field.
            let trimmed = out.trim_end().to_string();
            if let Some(pos) = trimmed.rfind(char::is_whitespace) {
                let (head, last_word) = trimmed.split_at(pos + 1);
                if LABEL_WORDS.contains(&last_word.trim().to_ascii_lowercase().as_str()) {
                    out = head.to_string();
                }
            }
        } else {
            out.push_str(&value);
        }
        rest = &tail[close + 1..];
    }
    out.push_str(rest);

    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let name = sanitize(&collapsed);
    if name.is_empty() {
        bail!(
            "name template {:?} produced an empty name for {:?}",
            cfg.name_template,
            meta
        );
    }
    Ok(name)
}

/// Filesystem-safe, printable, and short enough to survive any client.
pub fn sanitize(raw: &str) -> String {
    let mut s = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '/' | '\\' | ':' | '\0' => s.push('-'),
            c if c.is_control() => {}
            c => s.push(c),
        }
    }
    let mut s = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c| c == '.' || c == '-')
        .to_string();
    s = capitalize_first(&s);
    if s.chars().count() > 100 {
        s = s
            .chars()
            .take(100)
            .collect::<String>()
            .trim_end()
            .to_string();
    }
    s
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Make a name unique by appending ` (2)`, ` (3)`, … until `taken` says no.
/// `taken` sees both this run's claims and what is already on disk.
pub fn unique_name(taken: &impl Fn(&str) -> bool, name: &str) -> String {
    if !taken(name) {
        return name.to_string();
    }
    for k in 2..1000 {
        let candidate = format!("{name} ({k})");
        if !taken(&candidate) {
            return candidate;
        }
    }
    // Pathological. Let the caller fail on the collision instead of looping.
    format!("{name} ({})", 1000)
}

impl fmt::Display for Meta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.category)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg_with(template: &str) -> Config {
        let mut cfg = Config {
            input_dir: Default::default(),
            output_dir: Default::default(),
            llm_url: String::new(),
            llm_model: "m".into(),
            llm_api_key: String::new(),
            llm_timeout: 1,
            llm_retries: 1,
            llm_image_edge: 0,
            mock_llm: true,
            mock_llm_file: String::new(),
            max_long_edge: 1280,
            max_bytes: 307200,
            jpeg_quality_min: 62,
            jpeg_quality_max: 92,
            window: 6,
            overlap: 2,
            gap_hint: 90.0,
            keep_source: false,
            resume: true,
            describe_dirs: true,
            dry_run: true,
            best_effort: false,
            watch: false,
            watch_interval: 15,
            settle: 20,
            name_template: template.into(),
            unbranded: "Unbranded".into(),
            na_label: "N/A".into(),
            currency: "USD".into(),
            csv_name: "summary_{ts}.csv".into(),
            state_file: Default::default(),
            jobs: 1,
            log: "info".into(),
        };
        cfg.validate_test();
        cfg
    }

    trait ValidateForTest {
        fn validate_test(&mut self);
    }
    impl ValidateForTest for Config {
        fn validate_test(&mut self) {
            self.jobs = 1;
        }
    }

    fn meta(category: &str, color: &str, size: Option<&str>) -> Meta {
        Meta {
            category: category.into(),
            brand: None,
            color: color.into(),
            size: size.map(Into::into),
            details: String::new(),
            price: 10.0,
            weight_g: 200.0,
        }
    }

    /// The model returns brands in whatever case it happens to feel like, and
    /// they turn up in folder names and in the CSV.
    #[test]
    fn brand_is_capitalised_without_flattening() {
        assert_eq!(brand_case("pupjoy"), "Pupjoy");
        assert_eq!(brand_case("the north face"), "The North Face");
        // Already-capitalised words, including mixed case, are left alone.
        assert_eq!(brand_case("ANLU"), "ANLU");
        assert_eq!(brand_case("Levi's"), "Levi's");
        // A word that starts lowercase is capitalised even when the rest of it
        // is mixed: the model writes "pupjoy", not a brand style guide.
        assert_eq!(brand_case("iPhone"), "IPhone");
        assert_eq!(brand_case(""), "");
    }

    /// The shapes a real run produced, which is what the default template has
    /// to render: category first, then colour, then whatever distinguishes it,
    /// brand last, size last of all.
    #[test]
    fn default_template_reads_like_a_listing_title() {
        let cfg = cfg_with("{category} {brand} {details} {color} size {size}");
        let mut toy = meta("Dog toy", "white and orange", None);
        toy.brand = Some("Pupjoy".into());
        toy.details = "plush ring and bone".into();
        assert_eq!(
            render_name(&cfg, &toy).unwrap(),
            "Dog toy Pupjoy plush ring and bone white and orange"
        );

        // No visible brand: the gap closes up and the rest still reads.
        let mut charger = meta("Charger", "black", None);
        charger.details = "USB type-A and type-C".into();
        assert_eq!(
            render_name(&cfg, &charger).unwrap(),
            "Charger USB type-A and type-C black"
        );

        let mut wallet = meta("Wallet", "black", None);
        wallet.details = "leather with red star".into();
        assert_eq!(
            render_name(&cfg, &wallet).unwrap(),
            "Wallet leather with red star black"
        );

        // Everything empty but the category still gives a usable folder name.
        assert_eq!(render_name(&cfg, &meta("Belt", "", None)).unwrap(), "Belt");
    }

    #[test]
    fn default_template_gives_the_documented_shape() {
        let cfg = cfg_with("{category} {color} size {size}");
        assert_eq!(
            render_name(&cfg, &meta("Skirt", "blue", Some("S"))).unwrap(),
            "Skirt blue size S"
        );
    }

    #[test]
    fn empty_size_drops_the_word_size_too() {
        let cfg = cfg_with("{category} {color} size {size}");
        assert_eq!(
            render_name(&cfg, &meta("Wallet", "black", None)).unwrap(),
            "Wallet black"
        );
    }

    #[test]
    fn empty_colour_drops_its_label() {
        let cfg = cfg_with("{category} color {color}");
        assert_eq!(render_name(&cfg, &meta("Belt", "", None)).unwrap(), "Belt");
    }

    #[test]
    fn template_accepts_brand_and_rejects_nonsense() {
        let cfg = cfg_with("{brand} {category}");
        let mut m = meta("Jacket", "green", Some("M"));
        m.brand = Some("patagonia".into());
        assert_eq!(render_name(&cfg, &m).unwrap(), "Patagonia Jacket");
        let bad = cfg_with("{nonsense}");
        assert!(render_name(&bad, &m).is_err());
        let unterminated = cfg_with("{category");
        assert!(render_name(&unterminated, &m).is_err());
    }

    #[test]
    fn sanitize_handles_paths_and_length() {
        assert_eq!(sanitize("a/b:c"), "A-b-c");
        assert_eq!(sanitize("   spaced   out  "), "Spaced out");
        assert_eq!(
            sanitize("...leading and trailing..."),
            "Leading and trailing"
        );
        let long = "x".repeat(300);
        assert_eq!(sanitize(&long).chars().count(), 100);
    }

    #[test]
    fn unique_name_appends_counters() {
        let seen = std::cell::RefCell::new(std::collections::HashSet::new());
        let taken = |s: &str| {
            let mut g = seen.borrow_mut();
            if g.contains(s) {
                true
            } else {
                g.insert(s.to_string());
                false
            }
        };
        let a = unique_name(&taken, "Dress red");
        let b = unique_name(&taken, "Dress red");
        assert_eq!((a.as_str(), b.as_str()), ("Dress red", "Dress red (2)"));
    }

    #[test]
    fn verdict_parsing_is_strict_about_ids_and_length() {
        let img = |id: usize| SourceImage {
            path: Default::default(),
            name: format!("{id}.jpg"),
            id,
            epoch: None,
            time_source: crate::scan::TimeSource::None,
            bytes: 0,
            sha256: String::new(),
        };
        let (one, two) = (img(1), img(2));
        let window = vec![&one, &two];
        let good =
            json!({"images":[{"id":1,"verdict":"new","subject":"a"},{"id":"2","verdict":"same"}]});
        let vs = parse_verdicts(&good, &window).unwrap();
        assert_eq!(vs[0].boundary, Boundary::New);
        assert_eq!(vs[1].boundary, Boundary::Same);

        let wrong_id = json!({"images":[{"id":2,"verdict":"new"},{"id":1,"verdict":"same"}]});
        assert!(parse_verdicts(&wrong_id, &window).is_err());
        let short = json!({"images":[{"id":1,"verdict":"new"}]});
        assert!(parse_verdicts(&short, &window).is_err());
        let junk = json!({"images":[{"id":1,"verdict":"maybe"},{"id":2,"verdict":"same"}]});
        assert!(parse_verdicts(&junk, &window).is_err());
    }

    #[test]
    fn fallback_cuts_at_a_suspicious_gap_only() {
        let mk = |id: usize, epoch: i64| SourceImage {
            path: Default::default(),
            name: format!("{id}.jpg"),
            id,
            epoch: Some(epoch),
            time_source: crate::scan::TimeSource::FileName,
            bytes: 0,
            sha256: String::new(),
        };
        let (a, b, c) = (mk(1, 0), mk(2, 5), mk(3, 400));
        let window = vec![&a, &b, &c];

        let cuts = fallback_verdicts(&window, 90.0);
        assert_eq!(cuts[1].boundary, Boundary::Same);
        assert_eq!(cuts[2].boundary, Boundary::New);

        let (d, e, f) = (mk(1, 0), mk(2, 5), mk(3, 9));
        let tight = vec![&d, &e, &f];
        let cuts = fallback_verdicts(&tight, 90.0);
        assert!(cuts[1..].iter().all(|v| v.boundary == Boundary::Same));
    }

    #[test]
    fn meta_parsing_survives_strings_and_missing_colour() {
        let cfg = cfg_with("{category} {color} size {size}");
        let m = parse_meta(&json!({"category":"  Trainers ","brand":"","color":"White","size":"42","price":"18.50","weight_g":900}), &cfg).unwrap();
        assert_eq!(m.category, "Trainers");
        assert_eq!(m.brand, None);
        assert_eq!(m.color, "white");
        assert_eq!(m.size.as_deref(), Some("42"));
        assert_eq!(m.price, 18.5);
        assert_eq!(m.weight_g, 900.0);
        assert!(parse_meta(&json!({"category":"Bag"}), &cfg).is_err());
    }

    #[test]
    fn sampling_keeps_the_first_photo_and_spreads_the_rest() {
        let imgs: Vec<SourceImage> = (1..=20usize)
            .map(|i| SourceImage {
                path: Default::default(),
                name: format!("{i}"),
                id: i,
                epoch: None,
                time_source: crate::scan::TimeSource::None,
                bytes: 0,
                sha256: String::new(),
            })
            .collect();
        let s = sample(&imgs, 8);
        assert_eq!(s.len(), 8);
        // Both ends are in, and nothing doubles up.
        assert_eq!(s[0].id, 1);
        assert_eq!(s[7].id, 20);
        assert!(s.windows(2).all(|w| w[0].id < w[1].id));
        // A run shorter than the budget is passed through untouched.
        assert_eq!(sample(&imgs[..3], 8).len(), 3);
    }
}
