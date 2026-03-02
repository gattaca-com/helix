use refinery::{AsyncMigrate, Migrate, Report};

mod embedded_migrations {
    use refinery::embed_migrations;
    embed_migrations!("src/postgres/migrations");
}

pub fn run_migrations<C>(conn: &'_ mut C) -> Result<Report, Box<dyn std::error::Error>>
where
    C: Migrate,
{
    Ok(embedded_migrations::migrations::runner().run(conn)?)
}

pub async fn run_migrations_async<C>(conn: &'_ mut C) -> Result<Report, Box<dyn std::error::Error>>
where
    C: AsyncMigrate + Send,
{
    Ok(embedded_migrations::migrations::runner().run_async(conn).await?)
}
