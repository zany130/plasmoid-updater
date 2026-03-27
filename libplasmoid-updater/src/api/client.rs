// SPDX-License-Identifier: GPL-3.0-or-later
//
// API interaction based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// and KDE Discover (https://invent.kde.org/plasma/discover) -
// GPL-2.0-only OR GPL-3.0-only OR LicenseRef-KDE-Accepted-GPL

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use parking_lot::Mutex;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};

use crate::{
    types::{ComponentType, StoreEntry},
    {Error, Result},
};

use super::config::{ApiConfig, CONNECT_TIMEOUT, DEFAULT_API_CONFIG, REQUEST_TIMEOUT, USER_AGENT};
use super::ocs_parser::Meta;
use super::ocs_parser::{build_category_string, parse_ocs_response};

/// Parses a `Retry-After` header value (integer seconds) into milliseconds.
/// Returns `None` if the header is absent or not a valid non-negative integer.
fn parse_retry_after_ms(response: &reqwest::blocking::Response) -> Option<u32> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok())
        .map(|secs| secs.saturating_mul(1_000))
}

/// Thread-safe API client for KDE Store interactions.
#[derive(Clone)]
pub(crate) struct ApiClient {
    client: reqwest::blocking::Client,
    config: &'static ApiConfig,
    request_count: Arc<AtomicUsize>,
}

impl Default for ApiClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ApiClient {
    /// Creates a new API client with default configuration.
    ///
    /// # Panics
    ///
    /// Panics if the HTTP client cannot be created (e.g., TLS backend unavailable).
    pub fn new() -> Self {
        Self::with_config(&DEFAULT_API_CONFIG)
            .unwrap_or_else(|e| panic!("failed to create API client: {e}"))
    }

    /// Creates a new API client with the given configuration.
    pub(super) fn with_config(config: &'static ApiConfig) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;

        Ok(Self {
            client,
            config,
            request_count: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Returns a reference to the underlying HTTP client for reuse.
    pub fn http_client(&self) -> &reqwest::blocking::Client {
        &self.client
    }

    /// Total number of HTTP requests sent since this client was created.
    #[cfg(feature = "debug")]
    pub fn request_count(&self) -> usize {
        self.request_count.load(Ordering::Relaxed)
    }

    /// A shared handle to the request counter, suitable for passing to the installer.
    pub(crate) fn request_counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.request_count)
    }

    /// Builds a rayon thread pool limited to `max_concurrent_requests` threads.
    /// Falls back to a single-threaded pool if the build fails.
    fn build_request_pool(&self) -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new()
            .num_threads(self.config.max_concurrent_requests)
            .build()
            .unwrap_or_else(|e| {
                log::warn!(target: "api", "failed to build thread pool ({e}), falling back to single thread");
                rayon::ThreadPoolBuilder::new()
                    .num_threads(1)
                    .build()
                    .expect("failed to build single-thread fallback pool")
            })
    }

    /// Fetches all content from specified categories with parallel page fetching.
    pub fn fetch_all(&self, categories: &[ComponentType]) -> Result<Vec<StoreEntry>> {
        let category_str = build_category_string(categories);
        let base_url = self.config.base_url;
        let page_size = self.config.page_size;

        let first_url = format!(
            "{base_url}/content/data?categories={category_str}&page=0&pagesize={page_size}&sort=new"
        );

        let (first_entries, meta) = self.fetch_page(&first_url)?;
        let total_items = meta.total_items;

        if total_items <= u32::from(page_size) {
            return Ok(first_entries);
        }

        let total_pages = total_items.div_ceil(u32::from(page_size));
        let remaining_pages: Vec<u32> = (1..total_pages).collect();

        let all_entries = Arc::new(Mutex::new(first_entries));
        let errors = Arc::new(Mutex::new(Vec::new()));

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(self.config.max_concurrent_requests)
            .build()
            .unwrap_or_else(|_| rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap());

        pool.install(|| {
            remaining_pages.par_iter().for_each(|&page| {
                let url = format!(
                    "{base_url}/content/data?categories={category_str}&page={page}&pagesize={page_size}&sort=new"
                );

                match self.fetch_page(&url) {
                    Ok((entries, _)) => {
                        all_entries.lock().extend(entries);
                    }
                    Err(e) => {
                        errors.lock().push(e);
                    }
                }
            });
        });

        let errors = Arc::try_unwrap(errors).unwrap().into_inner();
        if !errors.is_empty() {
            log::warn!(target: "api", "{} page{} failed to fetch", errors.len(), if errors.len() == 1 { "" } else { "s" });
        }

        Ok(Arc::try_unwrap(all_entries).unwrap().into_inner())
    }

    /// Fetches content details of multiple components.
    pub fn fetch_details(&self, content_ids: &[u64]) -> Vec<Result<StoreEntry>> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(self.config.max_concurrent_requests)
            .build()
            .unwrap_or_else(|_| rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap());

        pool.install(|| {
            content_ids
                .par_iter()
                .map(|&id| {
                    let base_url = self.config.base_url;
                    let url = format!("{base_url}/content/data/{id}");
                    let (entries, _) = self.fetch_page(&url)?;
                    entries
                        .into_iter()
                        .next()
                        .ok_or_else(|| Error::ComponentNotFound(format!("store content id {id}")))
                })
                .collect()
        })
    }

    fn fetch_page(&self, url: &str) -> Result<(Vec<StoreEntry>, Meta)> {
        let mut backoff_ms = self.config.initial_backoff_ms;

        for attempt in 0..self.config.max_retries {
            self.request_count.fetch_add(1, Ordering::Relaxed);
            let resp = self.client.get(url).send()?;

            // Detect HTTP-level rate limiting (429 Too Many Requests).
            // Honor the Retry-After header when present; fall back to our backoff schedule.
            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let sleep_ms = parse_retry_after_ms(&resp).unwrap_or(backoff_ms);
                if attempt + 1 < self.config.max_retries {
                    log::warn!(
                        target: "api",
                        "rate limited (HTTP 429), retrying after {}ms (attempt {}/{})",
                        sleep_ms,
                        attempt + 1,
                        self.config.max_retries,
                    );
                    thread::sleep(Duration::from_millis(u64::from(sleep_ms)));
                    backoff_ms = backoff_ms
                        .saturating_mul(2)
                        .min(self.config.max_backoff_ms);
                    continue;
                }
                return Err(Error::RateLimited);
            }

            let xml = resp.text()?;
            match parse_ocs_response(&xml) {
                Ok(result) => return Ok(result),
                Err(Error::RateLimited) if attempt + 1 < self.config.max_retries => {
                    log::warn!(
                        target: "api",
                        "rate limited (OCS 200), retrying after {}ms (attempt {}/{})",
                        backoff_ms,
                        attempt + 1,
                        self.config.max_retries,
                    );
                    thread::sleep(Duration::from_millis(u64::from(backoff_ms)));
                    backoff_ms = backoff_ms
                        .saturating_mul(2)
                        .min(self.config.max_backoff_ms);
                }
                Err(_) if attempt + 1 < self.config.max_retries => {
                    thread::sleep(Duration::from_millis(u64::from(backoff_ms)));
                    backoff_ms = backoff_ms
                        .saturating_mul(2)
                        .min(self.config.max_backoff_ms);
                }
                Err(e) => return Err(e),
            }
        }

        Err(Error::other("max retries exceeded"))
    }
}
