pub use sqlx_core::{error::Error, query::query, query_as::query_as, query_scalar::query_scalar};
pub use sqlx_postgres::PgPool;

/// sqlx 0.9 accepts only `&'static str` as a query string
/// (`sqlx_core::sql_str::SqlSafeStr`); anything built at runtime must be
/// wrapped in `AssertSqlSafe`, which is the crate saying "I have audited this
/// string for injection". Re-exported here rather than imported from
/// `sqlx_core` at each call site so `AssertSqlSafe` is greppable as one name
/// across this crate — the wrap is an assertion about a query, and the set of
/// places making it is exactly what a security review needs to enumerate.
///
/// Every use in this crate today interpolates a compile-time `const` SQL
/// fragment and nothing else; each carries a comment naming what it
/// interpolates. **No value that originates from a request may reach a query
/// through this type** — bind parameters, which sqlx sends out of band from
/// the SQL text, are the only correct way to put caller data in a query.
pub use sqlx_core::sql_str::AssertSqlSafe;
/// The owned query string `AssertSqlSafe` (and `&'static str`) converts into.
/// Needed to build a [`migrate::Migration`], whose `sql` field is one.
pub use sqlx_core::sql_str::SqlStr;

pub mod postgres {
    pub use sqlx_postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
}

pub mod migrate {
    pub use sqlx_core::migrate::{Migration, MigrationType, Migrator};
}
