//! The vision model: one JSON in, one JSON out, with retries.
//!
//! The HTTP backend targets llama.cpp's OpenAI-compatible
//! `/chat/completions`, which is what `UNLOOT_LLM_URL` points at; anything else
//! that speaks the same shape works. Photos are downscaled before they are
//! base64'd — a 3 MB phone shot costs ~4 MB of prompt and buys nothing over a
//! 768 px copy.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::util;

/// One piece of a request: instructions, or a photo.
#[derive(Clone, Debug)]
pub enum Part {
    Text(String),
    /// `(label, jpeg bytes)` — the label is only used in logs.
    Image(String, Vec<u8>),
}

#[derive(Clone, Debug)]
pub struct Request<'a> {
    pub system: &'a str,
    pub parts: Vec<Part>,
    pub max_tokens: u32,
    pub temperature: f32,
    /// Free-form tag for logs, e.g. `boundary 3/5`.
    pub what: String,
}

/// Implemented by the HTTP backend and by the offline fixture.
///
/// `Send + Sync` because items are catalogued in parallel through one client.
pub trait Llm: Send + Sync {
    /// Ask for a JSON answer. Implementations must raise an error rather than
    /// return junk: callers retry and then fail loudly.
    fn complete_json(&self, req: &Request) -> Result<Value>;

    /// Ask the endpoint which model it has loaded. Only the HTTP backend can.
    fn detect_model(&self) -> Result<String> {
        Err(anyhow!("this LLM backend cannot list models"))
    }

    /// Fill in `--llm-model` from the endpoint when it was left empty, so the
    /// default ("") is a real "auto" rather than a request for no model.
    fn ensure_model(&self, model: &mut String) -> Result<bool> {
        if model.trim().is_empty() {
            *model = self.detect_model()?;
            return Ok(true);
        }
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// HTTP / OpenAI-compatible
// ---------------------------------------------------------------------------

pub struct HttpLlm {
    agent: ureq::Agent,
    url: String,
    model: String,
    api_key: String,
    retries: u32,
    /// Kept for the life of the run, not the request. A reasoning model that
    /// ran out of room once will run out again on the next photo, so every
    /// empty `finish_reason=length` answer raises the allowance for everything
    /// that follows rather than paying for the same mistake per prompt.
    boost: std::sync::atomic::AtomicU32,
}

impl HttpLlm {
    /// `base` is an OpenAI-compatible root such as `http://host:8010/v1`; a
    /// trailing `/chat/completions` is accepted too.
    /// Build a client for one explicit model. Reach for [`HttpLlm::resolve`],
    /// which fills in an empty model from the endpoint.
    fn with_model(cfg: &Config, model: &str) -> Result<Self> {
        if !cfg.llm_url.starts_with("http://") && !cfg.llm_url.starts_with("https://") {
            bail!("--llm-url must start with http:// or https://");
        }
        let url = if cfg.llm_url.ends_with("/chat/completions") {
            cfg.llm_url.clone()
        } else {
            format!("{}/chat/completions", cfg.llm_url)
        };
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(cfg.llm_timeout as u64)))
            .build();
        Ok(Self {
            agent: ureq::Agent::new_with_config(config),
            url,
            model: model.to_string(),
            api_key: cfg.llm_api_key.clone(),
            retries: cfg.llm_retries.max(1),
            boost: std::sync::atomic::AtomicU32::new(1),
        })
    }

    /// Build the client, asking the endpoint for its model name if we were not
    /// told one. Returns the client and whether the model was auto-detected.
    pub fn resolve(cfg: &Config) -> Result<(Self, bool)> {
        let probe = Self::with_model(cfg, "")?;
        let mut model = cfg.llm_model.clone();
        let auto = probe.ensure_model(&mut model)?;
        if auto {
            info!(model = %model, "using the model the endpoint reports; set --llm-model to pin it");
        }
        Ok((Self::with_model(cfg, &model)?, auto))
    }

    fn post(&self, body: &Value) -> Result<Value> {
        let payload = serde_json::to_string(body)?;
        let mut call = self
            .agent
            .post(&self.url)
            .header("content-type", "application/json");
        if !self.api_key.is_empty() {
            call = call.header("authorization", format!("Bearer {}", self.api_key));
        }
        let mut resp = call
            .send(payload.as_bytes())
            .with_context(|| format!("POST {}", self.url))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .with_context(|| format!("read response from {}", self.url))?;
        if !(200..300).contains(&status) {
            bail!(
                "LLM endpoint returned HTTP {status}: {}",
                text.chars().take(400).collect::<String>()
            );
        }
        let v: Value = serde_json::from_str(&text).context("endpoint reply was not JSON")?;
        Ok(v)
    }
}

/// Past this many completion tokens the doubling stops being worth the wait:
/// the next attempt drops the cap and lets the server decide.
const ROOM_CEILING: u32 = 16_384;
/// The boost at which we give up on capping and leave the length to the server.
/// Three doublings is enough: a reasoning model that needs more than four times
/// its budget on the first try is not going to be satisfied by eight either, and
/// each wasted attempt costs a full generation.
const ROOM_BOOST_MAX: u32 = 4;

impl Llm for HttpLlm {
    fn complete_json(&self, req: &Request) -> Result<Value> {
        let mut content = Vec::new();
        let mut photos = 0usize;
        for p in &req.parts {
            match p {
                Part::Text(t) => content.push(json!({"type": "text", "text": t})),
                Part::Image(label, bytes) => {
                    photos += 1;
                    debug!(photo = %label, kb = crate::util::kb(bytes.len()), "sending photo to the model");
                    content.push(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
                        }
                    }));
                }
            }
        }
        debug!(photos, what = %req.what, "LLM request assembled");

        // A reasoning model spends most of its budget thinking before it
        // answers, so a small max_tokens comes back as HTTP 200 with an empty
        // answer and finish_reason=length. Rather than fail, double the room on
        // the next attempt; when even that is not enough, stop sending a cap at
        // all and let the server use its own default. The boost is remembered
        // for the rest of the run, so one unlucky answer does not make every
        // later call pay for it again.
        let base = req.max_tokens.max(256);
        let boost = self.boost.load(std::sync::atomic::Ordering::Relaxed);
        // A previous answer already showed that even the ceiling is not enough,
        // so do not waste an attempt on a cap this call either.
        let mut uncapped = boost >= ROOM_BOOST_MAX;
        let mut budget = (base * boost.max(1)).min(ROOM_CEILING);
        let mut last_err = None;
        for attempt in 1..=self.retries {
            let mut body = json!({
                "model": self.model,
                "messages": [
                    {"role": "system", "content": req.system},
                    {"role": "user", "content": content},
                ],
                "temperature": req.temperature,
                // llama.cpp honours this; servers that ignore it are unaffected.
                "response_format": {"type": "json_object"},
                "stream": false,
            });
            if !uncapped {
                body["max_tokens"] = json!(budget);
            }
            let outcome = self.post(&body).and_then(|v| {
                reply_to_json(&v).inspect_err(|_e| {
                    if !ran_out_of_room(&v) {
                        return;
                    }
                    let grown = self.boost.load(std::sync::atomic::Ordering::Relaxed) * 2;
                    if uncapped || grown > ROOM_BOOST_MAX {
                        // Enough doubling. Ask for the answer without a cap and
                        // let the server's own limit decide when to stop.
                        uncapped = true;
                        self.boost.store(ROOM_BOOST_MAX, std::sync::atomic::Ordering::Relaxed);
                        warn!(what = %req.what, "the model spent its whole budget thinking again; leaving the length to the server from here on");
                    } else {
                        self.boost.store(grown, std::sync::atomic::Ordering::Relaxed);
                        budget = (base * grown).min(ROOM_CEILING);
                        warn!(budget, factor = grown, what = %req.what, "the model spent its whole budget thinking; allowing more from here on");
                    }
                })
            });
            match outcome {
                Ok(v) => return Ok(v),
                Err(e) => {
                    warn!(attempt, what = %req.what, error = %format!("{e:#}"), "LLM attempt failed");
                    last_err = Some(e);
                    if attempt < self.retries {
                        let backoff = Duration::from_millis(400 * (1u64 << (attempt - 1)) * 2);
                        std::thread::sleep(backoff.min(Duration::from_secs(8)));
                    }
                }
            }
        }
        Err(last_err.unwrap().context(format!(
            "LLM call {} failed after {} attempt(s)",
            req.what, self.retries
        )))
    }

    fn detect_model(&self) -> Result<String> {
        let base = self.url.trim_end_matches("/chat/completions");
        let url = format!("{base}/models");
        let mut call = self.agent.get(&url);
        if !self.api_key.is_empty() {
            call = call.header("authorization", format!("Bearer {}", self.api_key));
        }
        let v = call
            .call()?
            .body_mut()
            .read_json::<Value>()
            .with_context(|| format!("GET {url}"))?;
        // llama.cpp and OpenAI both answer; take the first id we can find.
        for path in [
            "/data/0/id",
            "/data/0/name",
            "/models/0/name",
            "/models/0/model",
        ] {
            if let Some(s) = v.pointer(path).and_then(|x| x.as_str())
                && !s.is_empty()
            {
                debug!(model = s, "auto-detected model");
                return Ok(s.to_string());
            }
        }
        bail!("could not find a model name in {url} reply")
    }
}

/// Pull the assistant message out of a chat response, then its JSON body.
///
/// `content` is a string on most servers and an array of parts on others; both
/// shapes appear in the wild and neither should fail the run.
fn reply_to_json(v: &Value) -> Result<Value> {
    let content = v
        .pointer("/choices/0/message/content")
        .and_then(|c| match c {
            Value::String(s) => Some(s.clone()),
            Value::Array(parts) => {
                let joined = parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("");
                Some(joined)
            }
            _ => None,
        })
        .ok_or_else(|| anyhow!("reply had no message content: {}", short(&v.to_string())))?;
    if content.trim().is_empty() {
        let finish = v
            .pointer("/choices/0/finish_reason")
            .and_then(|f| f.as_str())
            .unwrap_or("unknown");
        bail!("model returned an empty answer (finish_reason={finish})");
    }
    util::extract_json(&content).with_context(|| format!("in model reply {}", short(&content)))
}

/// True when the model stopped because it hit the token ceiling, which for a
/// reasoning endpoint usually means it thought itself empty rather than failed.
fn ran_out_of_room(v: &Value) -> bool {
    v.pointer("/choices/0/finish_reason")
        .and_then(|f| f.as_str())
        == Some("length")
}

fn short(s: &str) -> String {
    let t: String = s.chars().take(200).collect();
    if s.chars().count() > 200 {
        format!("{t}…")
    } else {
        t
    }
}

// ---------------------------------------------------------------------------
// Offline fixture
// ---------------------------------------------------------------------------

/// Deterministic stand-in for the endpoint.
///
/// With a fixture file it replays the JSON objects in `{"calls": [...]}` in
/// order, so tests can script exactly what the model would have said. Without
/// one it answers from the photo ids in the prompt (every 3rd photo starts a
/// new item), which is enough to exercise the whole pipeline offline.
pub struct MockLlm {
    calls: Vec<Value>,
    cursor: std::sync::atomic::AtomicUsize,
    path: Option<PathBuf>,
}

impl MockLlm {
    pub fn new(cfg: &Config) -> Result<Self> {
        let path = if cfg.mock_llm_file.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(cfg.mock_llm_file.trim()))
        };
        let calls = match &path {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .with_context(|| format!("read mock fixture {}", p.display()))?;
                let v: Value = serde_json::from_str(&text)
                    .with_context(|| format!("mock fixture {} is not JSON", p.display()))?;
                let arr = v
                    .get("calls")
                    .and_then(|c| c.as_array())
                    .cloned()
                    .unwrap_or_else(Vec::new);
                if arr.is_empty() {
                    bail!("mock fixture {} has no \"calls\" array", p.display());
                }
                arr
            }
            None => Vec::new(),
        };
        Ok(Self {
            calls,
            cursor: std::sync::atomic::AtomicUsize::new(0),
            path,
        })
    }
}

impl Llm for MockLlm {
    fn complete_json(&self, req: &Request) -> Result<Value> {
        if !self.calls.is_empty() {
            let i = self
                .cursor
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(v) = self.calls.get(i) {
                debug!(
                    call = i,
                    fixture = ?self.path.as_ref().map(|p| p.display().to_string()),
                    what = %req.what,
                    "mock LLM replay"
                );
                return Ok(v.clone());
            }
            bail!(
                "mock fixture ran out of canned calls at call #{i} ({} canned)",
                self.calls.len()
            );
        }
        // Heuristic mock: split whenever the prompt text says so.
        let flat = req
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                Part::Image(_, _) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if flat.contains("Describe the item") {
            Ok(json!({
                "category": "Wallet",
                "brand": null,
                "color": "black",
                "size": null,
                "details": "leather with red star",
                "price": 12.0,
                "weight_g": 120.0
            }))
        } else {
            Ok(json!({
                "images": flat_lines(&flat).into_iter().map(|(i, label, _)| json!({
                    "id": i,
                    "verdict": if label.contains("new") { "new" } else { "same" },
                    "subject": "item"
                })).collect::<Vec<_>>()
            }))
        }
    }

    fn detect_model(&self) -> Result<String> {
        Ok("mock-llm".into())
    }
}

/// Extract the `id=N ...` lines a boundary prompt is built from.
fn flat_lines(text: &str) -> Vec<(Value, String, f64)> {
    text.lines()
        .filter_map(|l| {
            let id = l.split("id=").nth(1)?.split([' ', ',']).next()?.to_string();
            let gap: f64 = l
                .split("gap=")
                .nth(1)?
                .split(['s', ' ', ','])
                .next()?
                .parse()
                .unwrap_or(0.0);
            Some((Value::from(id), l.to_string(), gap))
        })
        .collect()
}

/// Encode a source photo small enough to send to the model.
pub fn prepare_llm_image(path: &Path, long_edge: u32) -> Result<Vec<u8>> {
    if long_edge == 0 {
        return std::fs::read(path).with_context(|| format!("read {}", path.display()));
    }
    crate::imageops::encode_limited(path, long_edge, 220 * 1024, 60, 78).map(|o| o.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_content_is_a_retriable_error_not_a_panic() {
        let v = json!({"choices":[{"message":{"content":""},"finish_reason":"length"}]});
        let e = reply_to_json(&v).unwrap_err().to_string();
        assert!(e.contains("empty answer"), "{e}");
        assert!(e.contains("length"), "{e}");
    }

    #[test]
    fn missing_content_is_reported() {
        assert!(reply_to_json(&json!({"choices":[]})).is_err());
    }

    #[test]
    fn mock_splits_on_labels() {
        let m = MockLlm {
            calls: vec![],
            cursor: std::sync::atomic::AtomicUsize::new(0),
            path: None,
        };
        let req = Request {
            system: "",
            parts: vec![
                Part::Text("photo id=1 file=a.jpg gap=0s new".into()),
                Part::Text("photo id=2 file=b.jpg gap=1s".into()),
            ],
            max_tokens: 100,
            temperature: 0.0,
            what: "t".into(),
        };
        let v = m.complete_json(&req).unwrap();
        let imgs = v["images"].as_array().unwrap();
        assert_eq!(imgs.len(), 2);
        assert_eq!(imgs[0]["verdict"], "new");
        assert_eq!(imgs[1]["verdict"], "same");
    }
}
