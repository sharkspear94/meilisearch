use actix_web::http::header::{self};
use actix_web::{web, HttpRequest, HttpResponse};
use log::debug;
use serde::{Deserialize, Serialize};

use serde_json::json;
use time::OffsetDateTime;

use meilisearch_lib::index::{Settings, Unchecked};
use meilisearch_lib::MeiliSearch;
use meilisearch_types::error::ResponseError;
use meilisearch_types::star_or::StarOr;

use crate::analytics::Analytics;
use crate::extractors::authentication::{policies::*, GuardedData};
use prometheus::{Encoder, TextEncoder};

mod api_key;
mod dump;
pub mod indexes;
mod tasks;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(web::scope("/tasks").configure(tasks::configure))
        .service(web::resource("/health").route(web::get().to(get_health)))
        .service(web::scope("/keys").configure(api_key::configure))
        .service(web::scope("/dumps").configure(dump::configure))
        .service(web::resource("/stats").route(web::get().to(get_stats)))
        .service(web::resource("/version").route(web::get().to(get_version)))
        .service(web::scope("/indexes").configure(indexes::configure));
}

/// Extracts the raw values from the `StarOr` types and
/// return None if a `StarOr::Star` is encountered.
pub fn fold_star_or<T, O>(content: impl IntoIterator<Item = StarOr<T>>) -> Option<O>
where
    O: FromIterator<T>,
{
    content
        .into_iter()
        .map(|value| match value {
            StarOr::Star => None,
            StarOr::Other(val) => Some(val),
        })
        .collect()
}

const PAGINATION_DEFAULT_LIMIT: fn() -> usize = || 20;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Pagination {
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "PAGINATION_DEFAULT_LIMIT")]
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PaginationView<T> {
    pub results: Vec<T>,
    pub offset: usize,
    pub limit: usize,
    pub total: usize,
}

impl Pagination {
    /// Given the full data to paginate, returns the selected section.
    pub fn auto_paginate_sized<T>(
        self,
        content: impl IntoIterator<Item = T> + ExactSizeIterator,
    ) -> PaginationView<T>
    where
        T: Serialize,
    {
        let total = content.len();
        let content: Vec<_> = content
            .into_iter()
            .skip(self.offset)
            .take(self.limit)
            .collect();
        self.format_with(total, content)
    }

    /// Given an iterator and the total number of elements, returns the selected section.
    pub fn auto_paginate_unsized<T>(
        self,
        total: usize,
        content: impl IntoIterator<Item = T>,
    ) -> PaginationView<T>
    where
        T: Serialize,
    {
        let content: Vec<_> = content
            .into_iter()
            .skip(self.offset)
            .take(self.limit)
            .collect();
        self.format_with(total, content)
    }

    /// Given the data already paginated + the total number of elements, it stores
    /// everything in a [PaginationResult].
    pub fn format_with<T>(self, total: usize, results: Vec<T>) -> PaginationView<T>
    where
        T: Serialize,
    {
        PaginationView {
            results,
            offset: self.offset,
            limit: self.limit,
            total,
        }
    }
}

impl<T> PaginationView<T> {
    pub fn new(offset: usize, limit: usize, total: usize, results: Vec<T>) -> Self {
        Self {
            offset,
            limit,
            results,
            total,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "name")]
pub enum UpdateType {
    ClearAll,
    Customs,
    DocumentsAddition {
        #[serde(skip_serializing_if = "Option::is_none")]
        number: Option<usize>,
    },
    DocumentsPartial {
        #[serde(skip_serializing_if = "Option::is_none")]
        number: Option<usize>,
    },
    DocumentsDeletion {
        #[serde(skip_serializing_if = "Option::is_none")]
        number: Option<usize>,
    },
    Settings {
        settings: Settings<Unchecked>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessedUpdateResult {
    pub update_id: u64,
    #[serde(rename = "type")]
    pub update_type: UpdateType,
    pub duration: f64, // in seconds
    #[serde(with = "time::serde::rfc3339")]
    pub enqueued_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub processed_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedUpdateResult {
    pub update_id: u64,
    #[serde(rename = "type")]
    pub update_type: UpdateType,
    pub error: ResponseError,
    pub duration: f64, // in seconds
    #[serde(with = "time::serde::rfc3339")]
    pub enqueued_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub processed_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnqueuedUpdateResult {
    pub update_id: u64,
    #[serde(rename = "type")]
    pub update_type: UpdateType,
    #[serde(with = "time::serde::rfc3339")]
    pub enqueued_at: OffsetDateTime,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub started_processing_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum UpdateStatusResponse {
    Enqueued {
        #[serde(flatten)]
        content: EnqueuedUpdateResult,
    },
    Processing {
        #[serde(flatten)]
        content: EnqueuedUpdateResult,
    },
    Failed {
        #[serde(flatten)]
        content: FailedUpdateResult,
    },
    Processed {
        #[serde(flatten)]
        content: ProcessedUpdateResult,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexUpdateResponse {
    pub update_id: u64,
}

impl IndexUpdateResponse {
    pub fn with_id(update_id: u64) -> Self {
        Self { update_id }
    }
}

/// Always return a 200 with:
/// ```json
/// {
///     "status": "Meilisearch is running"
/// }
/// ```
pub async fn running() -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({ "status": "Meilisearch is running" }))
}

async fn get_stats(
    meilisearch: GuardedData<ActionPolicy<{ actions::STATS_GET }>, MeiliSearch>,
    req: HttpRequest,
    analytics: web::Data<dyn Analytics>,
) -> Result<HttpResponse, ResponseError> {
    analytics.publish(
        "Stats Seen".to_string(),
        json!({ "per_index_uid": false }),
        Some(&req),
    );
    let search_rules = &meilisearch.filters().search_rules;
    let response = meilisearch.get_all_stats(search_rules).await?;

    debug!("returns: {:?}", response);
    Ok(HttpResponse::Ok().json(response))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionResponse {
    commit_sha: String,
    commit_date: String,
    pkg_version: String,
}

async fn get_version(
    _meilisearch: GuardedData<ActionPolicy<{ actions::VERSION }>, MeiliSearch>,
) -> HttpResponse {
    let commit_sha = option_env!("VERGEN_GIT_SHA").unwrap_or("unknown");
    let commit_date = option_env!("VERGEN_GIT_COMMIT_TIMESTAMP").unwrap_or("unknown");

    HttpResponse::Ok().json(VersionResponse {
        commit_sha: commit_sha.to_string(),
        commit_date: commit_date.to_string(),
        pkg_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

#[derive(Serialize)]
struct KeysResponse {
    private: Option<String>,
    public: Option<String>,
}

pub async fn get_health() -> Result<HttpResponse, ResponseError> {
    Ok(HttpResponse::Ok().json(serde_json::json!({ "status": "available" })))
}

pub async fn get_metrics(
    meilisearch: GuardedData<ActionPolicy<{ actions::METRICS_GET }>, MeiliSearch>,
) -> Result<HttpResponse, ResponseError> {
    let search_rules = &meilisearch.filters().search_rules;
    let response = meilisearch.get_all_stats(search_rules).await?;

    crate::metrics::MEILISEARCH_DB_SIZE_BYTES.set(response.database_size as i64);
    crate::metrics::MEILISEARCH_INDEX_COUNT.set(response.indexes.len() as i64);

    for (index, value) in response.indexes.iter() {
        crate::metrics::MEILISEARCH_INDEX_DOCS_COUNT
            .with_label_values(&[index])
            .set(value.number_of_documents as i64);
    }

    let encoder = TextEncoder::new();
    let mut buffer = vec![];
    encoder
        .encode(&prometheus::gather(), &mut buffer)
        .expect("Failed to encode metrics");

    let response = String::from_utf8(buffer).expect("Failed to convert bytes to string");

    Ok(HttpResponse::Ok()
        .insert_header(header::ContentType(mime::TEXT_PLAIN))
        .body(response))
}
