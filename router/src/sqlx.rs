pub use sqlx_core::{error::Error, query::query, query_as::query_as, query_scalar::query_scalar};
pub use sqlx_postgres::PgPool;

pub mod postgres {
    pub use sqlx_postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
}

pub mod migrate {
    pub use sqlx_core::migrate::{Migration, MigrationType, Migrator};
}
