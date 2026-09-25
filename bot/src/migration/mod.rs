pub use sea_orm_migration::prelude::*;

mod m20250310_104146_create_tables;
mod m20250925_000001_create_favorites;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20250310_104146_create_tables::Migration),
            Box::new(m20250925_000001_create_favorites::Migration),
        ]
    }
}
