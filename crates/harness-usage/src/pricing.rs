//! Prices. A table shipped in the binary (a trimmed copy of models.dev's data, MIT), a table
//! `harness pricing update` stores, and the user's `[pricing."<glob>"]` entries, in that order of
//! precedence, field by field. A model in none of them has no price, and no cost.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use globset::GlobBuilder;
use harness_core::message::Buckets;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};

/// The snapshot shipped in the binary. Refresh it with `harness pricing update`, then copy
/// `pricing.json` over `data/pricing-snapshot.json`.
pub const EMBEDDED_JSON: &str = include_str!("../data/pricing-snapshot.json");

/// The providers whose prices are kept: those harness has built in.
pub const SUPPORTED: [&str; 3] = ["anthropic", "openai", "openrouter"];

/// Where `harness pricing update` fetches the data from.
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// The largest pricing data `harness pricing update` accepts.
const MAX_BYTES: usize = 32 * 1024 * 1024;

/// A model's prices, in USD per million tokens. Cache prices that are not given are the input
/// price (a cache read never costs more than a fresh read).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Price {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Writes to the cache at the 5-minute tier, or a tier the provider did not state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<f64>,
}

impl Price {
    /// This price with the fields `over` sets in place of its own.
    pub fn overlaid(&self, over: &Price) -> Price {
        Price {
            input: over.input.or(self.input),
            output: over.output.or(self.output),
            cache_read: over.cache_read.or(self.cache_read),
            cache_write: over.cache_write.or(self.cache_write),
            cache_write_1h: over.cache_write_1h.or(self.cache_write_1h),
        }
    }

    /// What `buckets` cost, in USD; `None` without an input and an output price. Reasoning is
    /// part of the output and is not priced again.
    pub fn cost(&self, buckets: &Buckets) -> Option<f64> {
        let input = self.input?;
        let output = self.output?;
        let read = self.cache_read.unwrap_or(input);
        let write = self.cache_write.unwrap_or(input);
        let write_1h = self.cache_write_1h.or(self.cache_write).unwrap_or(input);
        let per_million = buckets.input as f64 * input
            + buckets.cache_read as f64 * read
            + buckets.cache_write as f64 * write
            + buckets.cache_write_1h as f64 * write_1h
            + buckets.output as f64 * output;
        Some(per_million / 1e6)
    }
}

/// Prices by provider and model, with the date of the data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceTable {
    /// `YYYY-MM-DD`: when the data was taken.
    pub date: String,
    #[serde(default)]
    pub source: String,
    pub providers: BTreeMap<String, BTreeMap<String, Price>>,
}

impl PriceTable {
    /// The price of `model_id` (`<provider>/<model>`, the model part possibly holding `/`).
    pub fn get(&self, model_id: &str) -> Option<Price> {
        let (provider, model) = model_id.split_once('/')?;
        self.providers.get(provider)?.get(model).copied()
    }

    /// How many models have a price.
    pub fn model_count(&self) -> usize {
        self.providers.values().map(BTreeMap::len).sum()
    }
}

/// The snapshot shipped in the binary.
pub fn embedded() -> PriceTable {
    static TABLE: OnceLock<PriceTable> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            serde_json::from_str(EMBEDDED_JSON).expect("the shipped price snapshot is valid")
        })
        .clone()
}

/// Where a price came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The user's `[pricing]` entries (over a table, field by field).
    Override,
    /// The table `harness pricing update` stored, of this date.
    Downloaded(String),
    /// The snapshot in the binary, of this date.
    Embedded(String),
}

impl Source {
    /// The words the ledger and `/usage` use.
    pub fn label(&self) -> String {
        match self {
            Source::Override => "override".to_string(),
            Source::Downloaded(date) => format!("downloaded {date}"),
            Source::Embedded(date) => format!("embedded {date}"),
        }
    }
}

/// The three sources of prices.
#[derive(Debug, Clone)]
pub struct Pricing {
    embedded: PriceTable,
    downloaded: Option<PriceTable>,
    overrides: Vec<(String, Price)>,
}

impl Pricing {
    /// The shipped snapshot, the table stored at `downloaded` (when it exists and reads), and
    /// `overrides`: `[pricing."<glob>"]` entries, matched like model profiles.
    pub fn load(downloaded: &Path, overrides: Vec<(String, Price)>) -> Pricing {
        let downloaded = std::fs::read_to_string(downloaded)
            .ok()
            .and_then(|text| serde_json::from_str::<PriceTable>(&text).ok())
            .filter(|table| !table.date.is_empty());
        Pricing {
            embedded: embedded(),
            downloaded,
            overrides,
        }
    }

    /// The price of `model_id`, and where it came from. A subscription model has no table entry
    /// of its own and is priced as its API counterpart (`chatgpt/<id>` as `openai/<id>`).
    pub fn price_of(&self, model_id: &str) -> Option<(Price, Source)> {
        let counterpart = model_id
            .strip_prefix("chatgpt/")
            .map(|model| format!("openai/{model}"));
        let ids: Vec<&str> = std::iter::once(model_id)
            .chain(counterpart.as_deref())
            .collect();
        let from_table = ids.iter().find_map(|id| {
            self.downloaded
                .as_ref()
                .and_then(|t| t.get(id).map(|p| (p, Source::Downloaded(t.date.clone()))))
                .or_else(|| {
                    self.embedded
                        .get(id)
                        .map(|p| (p, Source::Embedded(self.embedded.date.clone())))
                })
        });
        let matching = self.matching_overrides(&ids);
        if matching.is_empty() {
            return from_table;
        }
        // Least specific first, so the most specific sets what it sets last.
        let base = from_table.map(|(price, _)| price).unwrap_or_default();
        let price = matching
            .iter()
            .rev()
            .fold(base, |price, over| price.overlaid(over));
        Some((price, Source::Override))
    }

    /// The overrides whose glob matches one of `ids`, the most specific first.
    fn matching_overrides(&self, ids: &[&str]) -> Vec<Price> {
        let mut found: Vec<(usize, Price)> = self
            .overrides
            .iter()
            .filter(|(glob, _)| {
                GlobBuilder::new(glob)
                    .case_insensitive(true)
                    .build()
                    .is_ok_and(|g| {
                        let matcher = g.compile_matcher();
                        ids.iter().any(|id| matcher.is_match(id))
                    })
            })
            .map(|(glob, price)| {
                (
                    glob.chars().filter(|c| !matches!(c, '*' | '?')).count(),
                    *price,
                )
            })
            .collect();
        // Stable: equally specific entries keep their order.
        found.sort_by_key(|(specificity, _)| std::cmp::Reverse(*specificity));
        found.into_iter().map(|(_, price)| price).collect()
    }

    /// Which table prices come from, with its date: the downloaded one, else the shipped one.
    pub fn snapshot(&self) -> Source {
        match &self.downloaded {
            Some(table) => Source::Downloaded(table.date.clone()),
            None => Source::Embedded(self.embedded.date.clone()),
        }
    }
}

/// Trims models.dev's `api.json` to what costs need: the supported providers' models that have an
/// input and an output price, each with its input, output, cache read and cache write prices.
/// Data that is not that shape, holds a negative price, or has no model to price is an error.
pub fn trim_models_dev(api_json: &str, date: &str) -> Result<PriceTable> {
    let invalid = |why: &str| Error(format!("that is not pricing data: {why}"));
    let root: Value = serde_json::from_str(api_json).map_err(|_| invalid("it is not JSON"))?;
    let root = root
        .as_object()
        .ok_or_else(|| invalid("it is not an object"))?;
    let mut providers = BTreeMap::new();
    for provider in SUPPORTED {
        let Some(models) = root
            .get(provider)
            .and_then(|p| p.get("models"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        let mut priced = BTreeMap::new();
        for (id, model) in models {
            let cost = &model["cost"];
            let (Some(input), Some(output)) = (cost["input"].as_f64(), cost["output"].as_f64())
            else {
                continue;
            };
            let price = Price {
                input: Some(input),
                output: Some(output),
                cache_read: cost["cache_read"].as_f64(),
                cache_write: cost["cache_write"].as_f64(),
                // models.dev names no 1-hour tier.
                cache_write_1h: None,
            };
            let all = [
                price.input,
                price.output,
                price.cache_read,
                price.cache_write,
            ];
            if all.iter().flatten().any(|p| !p.is_finite() || *p < 0.0) {
                return Err(invalid(&format!("{provider}/{id} has a negative price")));
            }
            priced.insert(id.clone(), price);
        }
        providers.insert(provider.to_string(), priced);
    }
    let table = PriceTable {
        date: date.to_string(),
        source: MODELS_DEV_URL.to_string(),
        providers,
    };
    if table.model_count() == 0 {
        return Err(invalid("it prices no model of a provider harness supports"));
    }
    Ok(table)
}

/// What an update stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Updated {
    pub date: String,
    pub models: usize,
    pub path: PathBuf,
}

/// Whether `url` may be fetched: over https, or from this machine (for tests).
fn allowed(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        || (url.scheme() == "http"
            && url.host_str().is_some_and(|host| {
                let host = host.trim_start_matches('[').trim_end_matches(']');
                host.eq_ignore_ascii_case("localhost")
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            }))
}

/// Fetches models.dev's data from `url`, validates it, and stores it at `dest` (private, written
/// through a temporary file, so a failure leaves the previous table in place). `today` is the
/// date the table is marked with. The only network connection harness makes for prices.
pub async fn update(url: &str, dest: &Path, today: &str) -> Result<Updated> {
    let url = reqwest::Url::parse(url).map_err(|e| Error(format!("`{url}` is not a URL: {e}")))?;
    if !allowed(&url) {
        return Err(Error(format!(
            "pricing data is fetched over https only, so {url} is refused"
        )));
    }
    let host = url.host_str().unwrap_or("the pricing host").to_string();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > 5 {
                attempt.error("too many redirects")
            } else if allowed(attempt.url()) {
                attempt.follow()
            } else {
                attempt.error("a redirect away from https")
            }
        }))
        .build()
        .map_err(|e| Error(format!("cannot start the HTTP client: {e}")))?;
    let fetch_error = |e: reqwest::Error| {
        let mut causes = Vec::new();
        let mut cause = std::error::Error::source(&e);
        while let Some(inner) = cause {
            causes.push(inner.to_string());
            cause = inner.source();
        }
        let mut why = e.without_url().to_string();
        for inner in causes {
            why.push_str(": ");
            why.push_str(&inner);
        }
        Error(format!("cannot fetch pricing data from {host}: {why}"))
    };
    let response = client.get(url.clone()).send().await.map_err(fetch_error)?;
    if !response.status().is_success() {
        return Err(Error(format!(
            "{host} answered HTTP {}: the pricing table is unchanged",
            response.status().as_u16()
        )));
    }
    if response
        .content_length()
        .is_some_and(|len| len > MAX_BYTES as u64)
    {
        return Err(Error(format!("the data from {host} is too large")));
    }
    let bytes = response.bytes().await.map_err(fetch_error)?;
    if bytes.len() > MAX_BYTES {
        return Err(Error(format!("the data from {host} is too large")));
    }
    let text = String::from_utf8(bytes.to_vec())
        .map_err(|_| Error("that is not pricing data: it is not text".into()))?;
    let table = trim_models_dev(&text, today)?;
    let json = serde_json::to_string(&table).map_err(|e| Error(e.to_string()))?;
    write_private(dest, json.as_bytes())?;
    Ok(Updated {
        date: table.date.clone(),
        models: table.model_count(),
        path: dest.to_path_buf(),
    })
}

/// Writes `bytes` to `path` through a temporary file in the same directory, private to the user.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    if let Some(dir) = path.parent() {
        crate::paths::create_private_dir(dir)?;
    }
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.map_err(Error::from)
}
