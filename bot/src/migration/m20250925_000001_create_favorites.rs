use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Favorite::Table)
                    .if_not_exists()
                    .col(big_integer(Favorite::Id).auto_increment().primary_key())
                    .col(big_integer(Favorite::ChatId))
                    .col(string(Favorite::Name))
                    .col(string(Favorite::Artist).default(""))
                    .col(string(Favorite::Album).default(""))
                    .col(timestamp(Favorite::CreatedAt).default(Expr::current_timestamp()))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .table(Favorite::Table)
                    .name("favorites_chat_name_artist")
                    .col(Favorite::ChatId)
                    .col(Favorite::Name)
                    .col(Favorite::Artist)
                    .unique()
                    .to_owned(),
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Favorite::Table).to_owned())
            .await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Favorite {
    Table,
    Id,
    ChatId,
    Name,
    Artist,
    Album,
    CreatedAt,
}
