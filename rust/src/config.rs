use clap::Parser;

#[derive(Parser, Debug)]
#[command(about = "Import OSM PBF bikelanes and roads into PostgreSQL")]
pub struct Config {
    /// Path to the .osm.pbf file
    #[arg(env = "PBF_FILE")]
    pub pbf_file: String,

    /// DB host; leave empty to connect via Unix socket (peer auth)
    #[arg(long, env = "PGHOST", default_value = "")]
    pub db_host: String,

    #[arg(long, env = "PGDATABASE", default_value = "postgres")]
    pub db_name: String,

    #[arg(long, env = "PGUSER", default_value = "postgres")]
    pub db_user: String,

    #[arg(long, env = "PGPASSWORD", default_value = "")]
    pub db_password: String,

    #[arg(long, env = "PGPORT", default_value_t = 5432)]
    pub db_port: u16,

    /// Truncate tables before import
    #[arg(long, default_value_t = true)]
    pub truncate: bool,
}
