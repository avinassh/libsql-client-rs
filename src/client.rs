//! `Client` is the main structure to interact with the database.

use async_trait::async_trait;

use anyhow::Result;

use super::{parse_query_result, QueryResult, Statement};

pub struct ClientConfig {
    url: String,
    backend: String,
}

/// Trait describing capabilities of a database client:
/// - executing statements, batches, transactions
#[async_trait(?Send)]
pub trait DatabaseClient {
    /// Executes a single SQL statement
    ///
    /// # Arguments
    /// * `stmt` - the SQL statement
    async fn execute(&self, stmt: impl Into<Statement>) -> Result<QueryResult> {
        let mut results = self.batch(std::iter::once(stmt)).await?;
        Ok(results.remove(0))
    }

    /// Executes a batch of SQL statements.
    /// Each statement is going to run in its own transaction,
    /// unless they're wrapped in BEGIN and END
    ///
    /// # Arguments
    /// * `stmts` - SQL statements
    async fn batch(
        &self,
        stmts: impl IntoIterator<Item = impl Into<Statement>>,
    ) -> Result<Vec<QueryResult>>;

    /// Executes an SQL transaction.
    /// Does not support nested transactions - do not use BEGIN or END
    /// inside a transaction.
    ///
    /// # Arguments
    /// * `stmts` - SQL statements
    async fn transaction(
        &self,
        stmts: impl IntoIterator<Item = impl Into<Statement>>,
    ) -> Result<Vec<QueryResult>> {
        let mut ret: Vec<QueryResult> = self
            .batch(
                std::iter::once(Statement::new("BEGIN"))
                    .chain(stmts.into_iter().map(|s| s.into()))
                    .chain(std::iter::once(Statement::new("END"))),
            )
            .await?
            .into_iter()
            .skip(1)
            .collect();
        ret.pop();
        Ok(ret)
    }
}

/// A generic client struct, wrapping possible backends.
/// It's a convenience struct which allows implementing connect()
/// with backends being passed as env parameters.
pub enum GenericClient {
    #[cfg(feature = "local_backend")]
    Local(super::local::Client),
    #[cfg(feature = "reqwest_backend")]
    Reqwest(super::reqwest::Client),
    #[cfg(feature = "workers_backend")]
    Workers(super::workers::Client),
    #[cfg(feature = "spin_backend")]
    Spin(super::spin::Client),
}

#[async_trait(?Send)]
impl DatabaseClient for GenericClient {
    async fn batch(
        &self,
        stmts: impl IntoIterator<Item = impl Into<Statement>>,
    ) -> Result<Vec<QueryResult>> {
        match self {
            #[cfg(feature = "local_backend")]
            Self::Local(l) => l.batch(stmts).await,
            #[cfg(feature = "reqwest_backend")]
            Self::Reqwest(r) => r.batch(stmts).await,
            #[cfg(feature = "workers_backend")]
            Self::Workers(w) => w.batch(stmts).await,
            #[cfg(feature = "spin_backend")]
            Self::Spin(s) => s.batch(stmts).await,
        }
    }
}

/// Establishes a database client based on environment variables
///
/// # Env
/// * `LIBSQL_CLIENT_URL` - URL of the database endpoint - e.g. a https:// endpoint for remote connections
///   (with specified credentials) or local file:/// path for a local database
/// * (optional) `LIBSQL_CLIENT_BACKEND` - one of the available backends,
///   e.g. `reqwest`, `local`, `workers`. The library will try to deduce the backend
///   from the URL if not set explicitly. For instance, it will assume that https:// is not a local file.
/// *
/// # Examples
///
/// ```
/// # use libsql_client::DatabaseClient;
/// # std::env::set_var("LIBSQL_CLIENT_URL", "file:////tmp/example.db");
/// let db = libsql_client::new_client().unwrap();
/// ```
pub fn new_client() -> anyhow::Result<GenericClient> {
    let url = std::env::var("LIBSQL_CLIENT_URL").map_err(|_| {
        anyhow::anyhow!("LIBSQL_CLIENT_URL variable should point to your libSQL/sqld database")
    })?;
    let backend = std::env::var("LIBSQL_CLIENT_BACKEND").unwrap_or_else(|_| {
        if url.starts_with("http") {
            return if cfg!(feature = "reqwest_backend") {
                "reqwest"
            } else if cfg!(feature = "workers_backend") {
                "workers"
            } else if cfg!(feature = "spin_backend") {
                "spin"
            } else {
                "local"
            }
            .to_string();
        } else {
            "local"
        }
        .to_string()
    });
    let config = ClientConfig { url, backend };
    new_client_with_config(&config)
}

/// Establishes a database client based on `Config` object.
pub fn new_client_with_config(config: &ClientConfig) -> anyhow::Result<GenericClient> {
    let url = config.url.to_owned();
    let backend = config.backend.to_owned();
    Ok(match backend.as_str() {
        #[cfg(feature = "local_backend")]
        "local" => {
            GenericClient::Local(super::local::Client::new(url)?)
        },
        #[cfg(feature = "reqwest_backend")]
        "reqwest" => {
            GenericClient::Reqwest(super::reqwest::Client::from_url(&url::Url::parse(&url)?)?)
        },
        #[cfg(feature = "workers_backend")]
        "workers" => {
            anyhow::bail!("Connecting from workers API may need access to worker::RouteContext. Please call libsql_client::workers::Client::connect_from_ctx() directly")
        },
        #[cfg(feature = "spin_backend")]
        "spin" => {
            anyhow::bail!("Connecting from spin API may need access to specific Spin SDK secrets. Please call libsql_client::spin::Client::connect_from_url() directly")
        },
        _ => anyhow::bail!("Unknown backend: {backend}. Make sure your backend exists and is enabled with its feature flag"),
    })
}

// FIXME: serialize and deserialize with existing routines from sqld
pub(crate) fn statements_to_string(
    stmts: impl IntoIterator<Item = impl Into<Statement>>,
) -> (String, usize) {
    let mut body = "{\"statements\": [".to_string();
    let mut stmts_count = 0;
    for stmt in stmts {
        body += &format!("{},", stmt.into());
        stmts_count += 1;
    }
    if stmts_count > 0 {
        body.pop();
    }
    body += "]}";
    (body, stmts_count)
}

pub(crate) fn json_to_query_result(
    response_json: serde_json::Value,
    stmts_count: usize,
) -> anyhow::Result<Vec<QueryResult>> {
    match response_json {
        serde_json::Value::Array(results) => {
            if results.len() != stmts_count {
                Err(anyhow::anyhow!(
                    "Response array did not contain expected {stmts_count} results"
                ))
            } else {
                let mut query_results: Vec<QueryResult> = Vec::with_capacity(stmts_count);
                for (idx, result) in results.into_iter().enumerate() {
                    query_results
                        .push(parse_query_result(result, idx).map_err(|e| anyhow::anyhow!("{e}"))?);
                }

                Ok(query_results)
            }
        }
        e => Err(anyhow::anyhow!("Error: {}", e)),
    }
}
