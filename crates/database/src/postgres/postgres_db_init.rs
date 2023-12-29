use refinery::{AsyncMigrate, Migrate, Report};

/// embeds the migrations folder into the binary
/// note: this macro cannot see if the migrations folder has changed
/// therefore cargo build will usually just use the cached version of the migrations
/// to force a rebuild of the migrations, e.g. run cargo clean or change the path to the migrations
/// folder
mod embedded_migrations {
    use refinery::embed_migrations;
    embed_migrations!("src/postgres/migrations");
}

/// Run the migrations defined in the /src/postgres/migrations folder
/// Migrations are executed in order of their version number
/// Refinery will automatically create a table called refinery_schema_history to track which
/// migrations have been run On running the migrations, refinery will check the database schema
/// history to determine which migrations to apply Expects a mutable reference to a connection that
/// implements the AsyncMigrate trait Specific to deadpool and tokio_postgres
pub fn run_migrations<C>(conn: &'_ mut C) -> Result<Report, Box<dyn std::error::Error>>
where
    C: Migrate,
{
    Ok(embedded_migrations::migrations::runner().run(conn)?)
}

/// Run the migrations defined in the /src/postgres/migrations folder
/// Migrations are executed in order of their version number
/// Refinery will automatically create a table called refinery_schema_history to track which
/// migrations have been run On running the migrations, refinery will check the database schema
/// history to determine which migrations to apply Expects a mutable reference to a connection that
/// implements the AsyncMigrate trait Specific to deadpool and tokio_postgres
pub async fn run_migrations_async<C>(conn: &'_ mut C) -> Result<Report, Box<dyn std::error::Error>>
where
    C: AsyncMigrate + Send,
{
    Ok(embedded_migrations::migrations::runner().run_async(conn).await?)
}
