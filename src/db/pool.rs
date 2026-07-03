use deadpool_postgres::{Config, ManagerConfig, Pool, RecyclingMethod, Runtime};
use tokio_postgres::NoTls;

use crate::config::Config as AppConfig;

pub fn build_pool(cfg: &AppConfig) -> anyhow::Result<Pool> {
    let mut pg_cfg = Config::new();
    pg_cfg.dbname = Some(cfg.db_name.clone());
    pg_cfg.user = Some(cfg.db_user.clone());

    // Use Unix socket (peer auth) when no explicit host is set, TCP otherwise.
    if cfg.db_host.is_empty() {
        pg_cfg.host = Some("/var/run/postgresql".to_owned());
    } else {
        pg_cfg.host = Some(cfg.db_host.clone());
        pg_cfg.port = Some(cfg.db_port);
        if !cfg.db_password.is_empty() {
            pg_cfg.password = Some(cfg.db_password.clone());
        }
    }

    pg_cfg.manager = Some(ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    });

    let pool = pg_cfg.create_pool(Some(Runtime::Tokio1), NoTls)?;
    Ok(pool)
}
