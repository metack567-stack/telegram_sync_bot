use super::entity::{chat_state, favorite, file_handle, file_state};
use super::state::*;
use crate::migration::{Migrator, MigratorTrait};
use anyhow::Result;
use sea_orm::ActiveValue::*;
use sea_orm::QueryOrder;
use sea_orm::TransactionTrait as _;
use sea_orm::prelude::*;
use sea_orm::sea_query;
use sea_orm::{Database, DatabaseConnection};
use tracing::info;

pub(super) async fn establish_connection(
    database_url: impl AsRef<str>,
) -> Result<DatabaseConnection> {
    info!("Connecting to database");
    let connection = Database::connect(database_url.as_ref()).await?;
    // refresh drops and recreates every table: only run it when explicitly
    // requested (RESET_DB=1), never implicitly in a debug build, or a dev
    // run against a real data dir would wipe all records.
    if std::env::var_os("RESET_DB").is_some() {
        Migrator::refresh(&connection).await?;
    } else {
        Migrator::up(&connection, None).await?;
    }

    info!("Connected to database");
    Ok(connection)
}

#[derive(Debug)]
pub(super) struct Db {
    db: DatabaseConnection,
}

impl Db {
    pub(super) async fn new(database_url: impl AsRef<str>) -> Result<Self> {
        let db = establish_connection(database_url.as_ref()).await?;
        info!(">> DB: connect to {}", database_url.as_ref());
        Ok(Self { db })
    }
}

impl Db {
    pub(super) async fn get_chat_state(&self, chat_id: i64) -> Result<ChatState> {
        match chat_state::Entity::find_by_id(chat_id)
            .one(&self.db)
            .await?
        {
            Some(m) => Ok(m.state.into()),
            None => Ok(ChatState::default()),
        }
    }

    pub(super) async fn toggle_chat_state(&self, chat_id: i64) -> Result<ChatState> {
        let txn = self.db.begin().await?;
        let current_state: ChatState =
            match chat_state::Entity::find_by_id(chat_id).one(&txn).await? {
                Some(m) => m.state.into(),
                None => ChatState::default(),
            };
        let new_state = current_state.toggle();
        chat_state::Entity::insert(chat_state::ActiveModel {
            chat_id: Set(chat_id),
            state: Set(new_state.to_string()),
        })
        .on_conflict(
            sea_query::OnConflict::column(chat_state::Column::ChatId)
                .update_column(chat_state::Column::State)
                .to_owned(),
        )
        .exec(&txn)
        .await?;
        txn.commit().await?;
        info!(
            ">> DB: set chat {} state from {} to {}",
            chat_id, current_state, new_state
        );
        Ok(new_state)
    }

    pub(super) async fn set_file_name(&self, file_id: String, file_name: String) -> Result<()> {
        file_state::Entity::insert(file_state::ActiveModel {
            file_id: Set(file_id.to_owned()),
            file_name: Set(file_name.to_owned()),
            ..Default::default()
        })
        .on_conflict(
            sea_query::OnConflict::column(file_state::Column::FileId)
                .update_column(file_state::Column::FileName)
                .to_owned(),
        )
        .exec(&self.db)
        .await?;
        info!(">> DB: set file {} name to {}", file_id, file_name);
        Ok(())
    }

    /// whether another file_id already uses this file_name
    pub(super) async fn file_name_taken(
        &self,
        file_id: &str,
        file_name: &str,
    ) -> Result<bool> {
        let n = file_state::Entity::find()
            .filter(file_state::Column::FileName.eq(file_name))
            .filter(file_state::Column::FileId.ne(file_id))
            .count(&self.db)
            .await?;
        Ok(n > 0)
    }

    pub(super) async fn get_transport_state(&self, file_id: String) -> Result<TransportState> {
        match file_state::Entity::find_by_id(file_id)
            .one(&self.db)
            .await?
        {
            Some(m) => Ok(m.transport_state.into()),
            None => Err(anyhow::anyhow!("File not found")),
        }
    }

    pub(super) async fn set_transport_state(
        &self,
        file_id: impl AsRef<str>,
        tranport_state: TransportState,
    ) -> Result<()> {
        file_state::Entity::insert(file_state::ActiveModel {
            file_id: Set(file_id.as_ref().to_string()),
            transport_state: Set(tranport_state.to_string()),
            ..Default::default()
        })
        .on_conflict(
            sea_query::OnConflict::column(file_state::Column::FileId)
                .update_column(file_state::Column::TransportState)
                .to_owned(),
        )
        .exec(&self.db)
        .await?;
        info!(
            ">> DB: set transport state {} to {}",
            file_id.as_ref(),
            tranport_state
        );
        Ok(())
    }

    /// all file ids/names whose transport state is still Downloading,
    /// used to resume downloads interrupted by a restart
    pub(super) async fn get_downloading_tasks(&self) -> Result<Vec<(String, String)>> {
        let rows = file_state::Entity::find()
            .filter(file_state::Column::TransportState.eq("Downloading"))
            .all(&self.db)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.file_id, r.file_name))
            .collect())
    }

    /// all file ids/names classified Normal, used by the /clear command to
    /// wipe the normal directory together with its db records
    pub(super) async fn get_normal_files(&self) -> Result<Vec<(String, String)>> {
        let rows = file_state::Entity::find()
            .filter(file_state::Column::State.eq("Normal"))
            .all(&self.db)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.file_id, r.file_name))
            .collect())
    }

    pub(super) async fn get_file_id_by_handle(&self, handle: (i64, i32)) -> Result<Option<String>> {
        match file_handle::Entity::find_by_id(handle)
            .one(&self.db)
            .await?
        {
            Some(m) => Ok(Some(m.file_id)),
            None => Ok(None),
        }
    }

    pub(super) async fn get_handle_by_file_id(
        &self,
        file_id: String,
    ) -> Result<Option<(i64, i32)>> {
        match file_handle::Entity::find()
            .filter(file_handle::Column::FileId.eq(file_id))
            .one(&self.db)
            .await?
        {
            Some(m) => Ok(Some((m.chat_id, m.msg_id))),
            None => Ok(None),
        }
    }

    pub(super) async fn get_file_state_and_name_by_handle(
        &self,
        handle: (i64, i32),
    ) -> Result<(FileState, String)> {
        let txn = self.db.begin().await?;
        let file_id = match file_handle::Entity::find_by_id(handle).one(&txn).await? {
            Some(m) => m.file_id,
            None => return Err(anyhow::anyhow!("File not found")),
        };
        let state = match file_state::Entity::find_by_id(file_id).one(&txn).await? {
            Some(m) => (m.state.into(), m.file_name),
            None => return Err(anyhow::anyhow!("File not found")),
        };
        txn.commit().await?;
        Ok(state)
    }

    pub(super) async fn set_file_state_by_handle_returning_old_state(
        &self,
        handle: (i64, i32),
        state: FileState,
    ) -> Result<Option<FileState>> {
        let txn = self.db.begin().await?;
        let file_id = match file_handle::Entity::find_by_id(handle).one(&txn).await? {
            Some(m) => m.file_id,
            None => return Err(anyhow::anyhow!("File not found")),
        };
        let old_state = file_state::Entity::find_by_id(file_id.to_owned())
            .one(&txn)
            .await?
            .map(|m| m.state.into());
        file_state::Entity::insert(file_state::ActiveModel {
            file_id: Set(file_id.to_owned()),
            state: Set(state.to_string()),
            ..Default::default()
        })
        .on_conflict(
            sea_query::OnConflict::column(file_state::Column::FileId)
                .update_column(file_state::Column::State)
                .to_owned(),
        )
        .exec(&txn)
        .await?;
        txn.commit().await?;
        info!(
            ">> DB: set file {} state from {:?} to {}",
            file_id, old_state, state
        );
        Ok(old_state)
    }

    /// Set file handle for a chat message, return the old handle if exists
    pub(super) async fn set_file_handle(
        &self,
        handle: (i64, i32),
        file_id: String,
    ) -> Result<Option<(i64, i32)>> {
        let txn = self.db.begin().await?;

        // insert file state if not exists, foreign key constraint
        file_state::Entity::insert(file_state::ActiveModel {
            file_id: Set(file_id.to_owned()),
            ..Default::default()
        })
        .exec(&txn)
        .await
        .ok();

        let result = match file_handle::Entity::find()
            .filter(file_handle::Column::ChatId.eq(handle.0))
            .filter(file_handle::Column::FileId.eq(file_id.to_owned()))
            .one(&txn)
            .await?
        {
            Some(m) if m.msg_id != handle.1 => {
                let old_chat_id = m.chat_id;
                let old_msg_id = m.msg_id;
                file_handle::Entity::delete(file_handle::ActiveModel {
                    chat_id: Set(old_chat_id),
                    msg_id: Set(old_msg_id),
                    ..Default::default()
                })
                .exec(&txn)
                .await?;
                let mut m: file_handle::ActiveModel = m.into();
                m.msg_id = Set(handle.1);
                file_handle::Entity::insert(m).exec(&txn).await?;
                Some((old_chat_id, old_msg_id))
            }
            Some(_) => None,
            None => {
                file_handle::Entity::insert(file_handle::ActiveModel {
                    chat_id: Set(handle.0),
                    msg_id: Set(handle.1),
                    file_id: Set(file_id.to_owned()),
                })
                .exec(&txn)
                .await?;
                None
            }
        };
        txn.commit().await?;
        info!(">> DB: set file {} handle {:?}", file_id, handle);
        Ok(result)
    }

    pub(super) async fn get_file_ids_by_name(&self, file_name: String) -> Result<Vec<String>> {
        let file_ids = file_state::Entity::find()
            .filter(file_state::Column::FileName.eq(file_name))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|m| m.file_id)
            .collect();
        Ok(file_ids)
    }

    pub(super) async fn delete_file_record(&self, file_id: String) -> Result<()> {
        let txn = self.db.begin().await?;
        file_handle::Entity::delete_many()
            .filter(file_handle::Column::FileId.eq(file_id.to_owned()))
            .exec(&txn)
            .await?;
        file_state::Entity::delete_by_id(file_id.to_owned())
            .exec(&txn)
            .await?;
        txn.commit().await?;
        info!(">> DB: delete file {}", file_id);
        Ok(())
    }

    pub(super) async fn delete_handle(&self, (chat_id, msg_id): (i64, i32)) -> Result<()> {
        file_handle::Entity::delete(file_handle::ActiveModel {
            chat_id: Set(chat_id),
            msg_id: Set(msg_id),
            ..Default::default()
        })
        .exec(&self.db)
        .await?;
        Ok(())
    }

    // ----- favorites（音乐收藏，与文件传输无关） -----

    /// 新增收藏；已存在（同 chat+歌名+歌手）时返回 false，不重复插入。
    pub(super) async fn add_favorite(
        &self,
        chat_id: i64,
        name: &str,
        artist: &str,
        album: &str,
    ) -> Result<bool> {
        let exists = favorite::Entity::find()
            .filter(favorite::Column::ChatId.eq(chat_id))
            .filter(favorite::Column::Name.eq(name))
            .filter(favorite::Column::Artist.eq(artist))
            .count(&self.db)
            .await?;
        if exists > 0 {
            return Ok(false);
        }
        favorite::Entity::insert(favorite::ActiveModel {
            chat_id: Set(chat_id),
            name: Set(name.to_string()),
            artist: Set(artist.to_string()),
            album: Set(album.to_string()),
            ..Default::default()
        })
        .exec(&self.db)
        .await?;
        info!(">> DB: favorite added chat {} {} - {}", chat_id, name, artist);
        Ok(true)
    }

    /// 取消收藏；存在并删除返回 true，不存在返回 false。
    pub(super) async fn remove_favorite(
        &self,
        chat_id: i64,
        name: &str,
        artist: &str,
    ) -> Result<bool> {
        let res = favorite::Entity::delete_many()
            .filter(favorite::Column::ChatId.eq(chat_id))
            .filter(favorite::Column::Name.eq(name))
            .filter(favorite::Column::Artist.eq(artist))
            .exec(&self.db)
            .await?;
        let removed = res.rows_affected > 0;
        if removed {
            info!(">> DB: favorite removed chat {} {} - {}", chat_id, name, artist);
        }
        Ok(removed)
    }

    /// 按收藏时间倒序列出某 chat 的收藏。
    pub(super) async fn list_favorites(&self, chat_id: i64) -> Result<Vec<FavoriteRow>> {
        let rows = favorite::Entity::find()
            .filter(favorite::Column::ChatId.eq(chat_id))
            .order_by_desc(favorite::Column::CreatedAt)
            .all(&self.db)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| FavoriteRow {
                name: r.name,
                artist: r.artist,
                album: r.album,
                created_at: r.created_at.format("%Y-%m-%d %H:%M").to_string(),
            })
            .collect())
    }
}

/// 一条音乐收藏记录（不包含文件句柄，收藏的是"歌名/歌手/专辑"元数据）。
#[derive(Debug, Clone)]
pub struct FavoriteRow {
    pub name: String,
    pub artist: String,
    pub album: String,
    pub created_at: String,
}
