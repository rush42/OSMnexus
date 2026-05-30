use thiserror::Error;

#[derive(Error, Debug)]
pub enum PipelineError {
    #[error("PBF read error: {0}")]
    Pbf(#[from] osmpbf::Error),

    #[error("Database error: {0}")]
    Db(#[from] tokio_postgres::Error),

    #[error("Pool error: {0}")]
    Pool(#[from] deadpool_postgres::PoolError),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}
