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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::embedded_migrations;

    /// Two files at one version make refinery depend on directory read order.
    #[test]
    fn migration_versions_are_unique() {
        let mut by_version: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        for migration in embedded_migrations::migrations::runner().get_migrations() {
            by_version.entry(migration.version()).or_default().push(migration.name().to_string());
        }

        let duplicated: Vec<_> = by_version.iter().filter(|(_, names)| names.len() > 1).collect();
        assert!(duplicated.is_empty(), "migration versions must be unique, found {duplicated:?}");
    }
}
