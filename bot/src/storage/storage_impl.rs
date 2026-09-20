use super::transport::TransportHandle;
use super::{FileId, FileName};
use super::{db::Db, state::*, transport::Downloader};
use crate::context::Context;
use anyhow::{Result, anyhow};
use std::path::PathBuf;
use std::sync::Arc;
use teloxide::Bot;
use teloxide::types::{ChatId, MessageId};
use tokio::fs;
use tracing::{info, instrument, warn};

/// map a file name to the media category subdir under `normal/`.
/// Telegram gives most files an extension (photo -> .jpg, video -> .mp4,
/// document -> original name), so extension-based classification is reliable.
fn classify_file_name(file_name: &str) -> &'static str {
    let ext = file_name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "heif" | "bmp"
        | "svg" | "tif" | "tiff" | "avif" | "ico" => "images",
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" | "ts" | "flv"
        | "wmv" | "3gp" | "mpeg" | "mpg" | "rmvb" => "videos",
        "mp3" | "flac" | "wav" | "m4a" | "aac" | "ogg" | "opus" | "wma"
        | "amr" | "ape" => "audio",
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt"
        | "md" | "csv" | "zip" | "rar" | "7z" | "tar" | "gz" | "bz2"
        | "xz" | "epub" | "apk" | "exe" | "iso" | "dmg" | "deb" | "rpm"
        | "json" | "xml" => "documents",
        _ => "other",
    }
}

#[derive(Debug, Clone)]
pub struct MyStorage {
    // Arc<Db> keeps MyStorage cheaply cloneable even when DatabaseConnection
    // itself is not Clone (e.g. under the `mock` feature), so --all-features
    // builds (and CI clippy) pass
    db: Arc<Db>,
    downloader: Arc<Downloader>,
    context: Context,
    file_name_lock: Arc<tokio::sync::Mutex<()>>,
}

impl MyStorage {
    pub async fn new(database_url: impl AsRef<str>, bot: Bot, context: Context) -> Result<Self> {
        let db = Arc::new(Db::new(database_url).await?);
        let downloader = Arc::new(Downloader::new(bot, context.clone()));
        Ok(Self {
            db,
            downloader,
            context,
            file_name_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }
}

impl MyStorage {
    pub async fn get_chat_state(&self, chat_id: ChatId) -> Result<ChatState> {
        self.db.get_chat_state(chat_id.0).await
    }

    pub async fn toggle_chat_state(&self, chat_id: ChatId) -> Result<ChatState> {
        self.db.toggle_chat_state(chat_id.0).await
    }

    pub async fn get_file_state_by_handle(&self, handle: (ChatId, MessageId)) -> Result<FileState> {
        self.db
            .get_file_state_and_name_by_handle((handle.0.0, handle.1.0))
            .await
            .map(|state_name| state_name.0)
    }

    /// 1. hard link file to correct directory
    /// 2. set file state by handle
    #[instrument(level = "debug", fields(file_name, old_state))]
    pub async fn set_file_state_by_handle_and_link(
        &self,
        handle: (ChatId, MessageId),
        state: FileState,
    ) -> Result<()> {
        let (old_state, file_name) = self
            .db
            .get_file_state_and_name_by_handle((handle.0.0, handle.1.0))
            .await?;

        tracing::Span::current().record("file_name", &file_name);
        tracing::Span::current().record("old_state", old_state.to_string());

        let dir = self.context.data_dir.join(handle.0.to_string());
        let new_dir = match state {
            FileState::Normal => dir.join("normal").join(classify_file_name(&file_name)),
            other => dir.join(other.to_string().to_lowercase()),
        };
        fs::create_dir_all(&new_dir).await?;
        let old_base = dir.join(old_state.to_string().to_lowercase());
        // Normal files live in normal/<category>/, but pre-classification
        // files may still sit directly in normal/ — check both.
        let from = match old_state {
            FileState::Normal => {
                let typed = old_base
                    .join(classify_file_name(&file_name))
                    .join(&file_name);
                if tokio::fs::try_exists(&typed).await.unwrap_or(false) {
                    typed
                } else {
                    old_base.join(&file_name)
                }
            }
            _ => old_base.join(&file_name),
        };
        let to = new_dir.join(&file_name);
        match fs::rename(&from, &to).await {
            Ok(_) => {
                info!(
                    ">> STORAGE: moved file from {} to {}",
                    from.display(),
                    to.display()
                );
                self.db
                    .set_file_state_by_handle_returning_old_state((handle.0.0, handle.1.0), state)
                    .await?;
            }
            Err(_) => {
                let origin = self.context.data_dir.join(&file_name);
                let db = self.db.clone();
                tokio::spawn(async move {
                    // wait until the flat-layer file appears (download finished) or
                    // the download task reaches a terminal state (failed/cancelled),
                    // whichever comes first. The safety cap below only guards against
                    // a stuck DB state and is far above any realistic download time.
                    let file_id = match db
                        .get_file_id_by_handle((handle.0.0, handle.1.0))
                        .await
                    {
                        Ok(Some(fid)) => fid,
                        _ => {
                            // handle record missing (message deleted / task cancelled)
                            // or db error: waiting is pointless
                            return Err(anyhow!(
                                "no handle record, skip classification for {}",
                                file_name
                            ));
                        }
                    };
                    for _ in 0..14400 {
                        // at most ~12 hours, only reached if the DB state never updates
                        if fs::try_exists(&origin).await? {
                            fs::hard_link(&origin, &to).await?;
                            // remove the flat-layer hard link, keep only the one in the
                            // classified directory to avoid duplicate entries
                            if origin != to {
                                fs::remove_file(origin).await.ok();
                            }
                            if from != to {
                                fs::remove_file(from).await.ok();
                            }
                            db.set_file_state_by_handle_returning_old_state(
                                (handle.0.0, handle.1.0),
                                state,
                            )
                            .await?;
                            return Ok::<_, anyhow::Error>(());
                        }
                        // give up as soon as the download failed or was cancelled:
                        // the file will never appear, waiting would be pointless
                        if matches!(
                            db.get_transport_state(file_id.clone()).await,
                            Ok(TransportState::Failed) | Ok(TransportState::Cancelled)
                        ) {
                            return Err(anyhow!(
                                "download failed or cancelled, skip classification for {}",
                                file_name
                            ));
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    }
                    Err(anyhow!("File not exists {}", file_name))
                });
            }
        }

        Ok(())
    }

    pub async fn get_handle_by_file_id(
        &self,
        file_id: FileId,
    ) -> Result<Option<(ChatId, MessageId)>> {
        Ok(self
            .db
            .get_handle_by_file_id(file_id)
            .await?
            .map(|(chat_id, msg_id)| (ChatId(chat_id), MessageId(msg_id))))
    }

    pub async fn get_file_ids_by_name(&self, file_name: String) -> Result<Vec<FileId>> {
        self.db.get_file_ids_by_name(file_name).await
    }

    /// set file_handle for file_id, return old handle if exists
    pub async fn set_file_handle(
        &self,
        chat_id: ChatId,
        msg_id: MessageId,
        file_id: FileId,
    ) -> Result<Option<(ChatId, MessageId)>> {
        let old_handle = self
            .db
            .set_file_handle((chat_id.0, msg_id.0), file_id)
            .await?;
        Ok(old_handle.map(|(chat_id, msg_id)| (ChatId(chat_id), MessageId(msg_id))))
    }

    pub async fn delete_file_record(&self, file_id: FileId) -> Result<()> {
        self.db.delete_file_record(file_id).await
    }

    pub async fn delete_handle(&self, handle: (ChatId, MessageId)) -> Result<()> {
        self.db.delete_handle((handle.0.0, handle.1.0)).await
    }
}

impl MyStorage {
    /// add a download task, return a handle
    pub async fn add_task(
        &self,
        file_id: FileId,
        file_name: FileName,
    ) -> Result<Option<TransportHandle>> {
        info!(">> Storage: Add new task {}", file_name);
        if matches!(
            self.db.get_transport_state(file_id.to_string()).await,
            Ok(TransportState::Completed)
        ) {
            info!(">> Storage: already finished");
            return Ok(None);
        }
        // assign a unique file name: if another file_id already holds this name,
        // append `_1`, `_2` ... so files never overwrite each other
        let final_name = {
            let _guard = self.file_name_lock.lock().await;
            let mut name = file_name;
            if self.db.file_name_taken(&file_id, &name).await? {
                name = self.unique_file_name(&file_id, &name).await?;
            }
            self.db
                .set_file_name(file_id.clone(), name.clone())
                .await?;
            name
        };
        let handle = self.downloader.add(file_id.clone(), final_name);
        let db = self.db.clone();
        let downloader_c = self.downloader.clone();
        let handle_c = handle.clone();
        tokio::spawn(async move {
            while TransportState::Pending == handle_c.get_state() {
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            }

            if !handle_c.is_cancelled() {
                db.set_transport_state(file_id.clone(), handle_c.get_state())
                    .await?;
            }

            db.set_transport_state(file_id.clone(), handle_c.result().await)
                .await?;
            // drop the finished handle so the map does not grow forever
            downloader_c.remove(&file_id, &handle_c);
            Ok::<_, anyhow::Error>(())
        });
        Ok(Some(handle))
    }

    /// find a free file name like `stem_1.ext`, `stem_2.ext` ... when `base` is taken
    async fn unique_file_name(&self, file_id: &str, base: &str) -> Result<String> {
        let (stem, ext) = match base.rsplit_once('.') {
            Some((s, e)) if !e.is_empty() => (s.to_string(), format!(".{}", e)),
            _ => (base.to_string(), String::new()),
        };
        for i in 1..=9999 {
            let candidate = format!("{}_{}{}", stem, i, ext);
            let taken = self.db.file_name_taken(file_id, &candidate).await?;
            let exists = tokio::fs::try_exists(self.context.data_dir.join(&candidate))
                .await
                .unwrap_or(false);
            if !taken && !exists {
                return Ok(candidate);
            }
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Ok(format!("{}_{}{}", stem, ts, ext))
    }

    /// re-queue downloads left in Downloading state by a restart. If the
    /// local server cache already holds the file, get_file resolves
    /// instantly and the file gets linked into place and classified.
    pub async fn resume_downloads(&self) -> Result<usize> {
        let tasks = self.db.get_downloading_tasks().await?;
        for (file_id, file_name) in &tasks {
            if let Some(handle) = self
                .add_task(file_id.clone(), file_name.clone())
                .await?
            {
                let storage = self.clone();
                let handle_c = handle.clone();
                let file_id_c = file_id.clone();
                tokio::spawn(async move {
                    if handle_c.result().await == TransportState::Completed {
                        if let Ok(Some((chat_id, msg_id))) = storage
                            .get_handle_by_file_id(file_id_c)
                            .await
                        {
                            storage
                                .set_file_state_by_handle_and_link(
                                    (chat_id, msg_id),
                                    FileState::Normal,
                                )
                                .await
                                .ok();
                        }
                    }
                });
            }
        }
        Ok(tasks.len())
    }

    /// on-disk candidate locations of a Normal-classified file: the flat
    /// layer (where downloads land) and the normal subdir of its chat
    fn normal_file_paths(&self, file_name: &str, handle: Option<&(i64, i32)>) -> Vec<PathBuf> {
        let mut paths = vec![self.context.data_dir.join(file_name)];
        if let Some((chat_id, _)) = handle {
            let normal = self.context.data_dir.join(chat_id.to_string()).join("normal");
            paths.push(normal.join(classify_file_name(file_name)).join(file_name));
            // legacy: files classified before category dirs existed
            paths.push(normal.join(file_name));
        }
        paths
    }

    /// number of files classified Normal and their total on-disk bytes
    pub async fn summarize_normal(&self) -> Result<(usize, u64)> {
        let files = self.db.get_normal_files().await?;
        let mut bytes = 0u64;
        for (file_id, file_name) in &files {
            let handle = self.db.get_handle_by_file_id(file_id.clone()).await?;
            for path in self.normal_file_paths(file_name, handle.as_ref()) {
                if let Ok(md) = tokio::fs::metadata(&path).await {
                    if md.is_file() {
                        bytes += md.len();
                    }
                }
            }
        }
        Ok((files.len(), bytes))
    }

    /// cancel running downloads, then remove all Normal files (classified
    /// dir + flat layer) together with their db records. Returns
    /// (files_cleared, bytes_freed).
    pub async fn clear_normal(&self) -> Result<(usize, u64)> {
        let files = self.db.get_normal_files().await?;
        let mut freed = 0u64;
        let mut seen = std::collections::HashSet::new();
        for (file_id, file_name) in &files {
            // cancel a running download first so it does not recreate the file
            if let Ok(Some((chat_id, msg_id))) = self
                .db
                .get_handle_by_file_id(file_id.clone())
                .await
            {
                let _ = self
                    .cancel_task_by_handle(ChatId(chat_id), MessageId(msg_id))
                    .await;
            }
            let handle = self.db.get_handle_by_file_id(file_id.clone()).await?;
            for path in self.normal_file_paths(file_name, handle.as_ref()) {
                let Ok(md) = tokio::fs::metadata(&path).await else { continue };
                if !md.is_file() {
                    continue;
                }
                // hard links share one inode: count the bytes only once
                use std::os::unix::fs::MetadataExt;
                let key = (md.dev(), md.ino());
                if seen.insert(key) {
                    freed += md.len();
                }
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    warn!(">> CLEAR: failed to remove {}: {}", path.display(), e);
                }
            }
            self.db.delete_file_record(file_id.clone()).await.ok();
        }
        Ok((files.len(), freed))
    }

    /// cancel a download task
    pub async fn cancel_task_by_handle(&self, chat_id: ChatId, msg_id: MessageId) -> Result<()> {
        match self.db.get_file_id_by_handle((chat_id.0, msg_id.0)).await? {
            Some(file_id) => {
                self.downloader.cancel(file_id);
                Ok(())
            }
            None => Err(anyhow!("No such handle")),
        }
    }

    /// delete trash files older than `retention_days`, together with their DB records.
    /// Returns the number of files removed.
    pub async fn cleanup_trash(&self, retention_days: u64) -> Result<u32> {
        let cutoff = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(
                retention_days.saturating_mul(24 * 3600),
            ))
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let mut removed = 0u32;
        let base = self.context.data_dir.clone();
        let mut chats = tokio::fs::read_dir(&base).await?;
        while let Some(chat) = chats.next_entry().await? {
            if !chat.file_type().await?.is_dir() {
                continue;
            }
            let trash = chat.path().join("trash");
            if !tokio::fs::try_exists(&trash).await? {
                continue;
            }
            let mut files = tokio::fs::read_dir(&trash).await?;
            while let Some(entry) = files.next_entry().await? {
                if !entry.file_type().await?.is_file() {
                    continue;
                }
                // use ctime (not mtime): rename/hard-link updates ctime, so a
                // file downloaded long ago but only trashed today is counted
                // from the moment it entered the trash.
                use std::os::unix::fs::MetadataExt;
                let md = entry.metadata().await?;
                let trash_time = std::time::SystemTime::UNIX_EPOCH
                    .checked_add(std::time::Duration::from_secs(md.ctime().max(0) as u64))
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                if trash_time > cutoff {
                    continue;
                }
                let file_name = entry.file_name().to_string_lossy().to_string();
                // drop the DB records for this file name, they are no longer valid
                for file_id in self.db.get_file_ids_by_name(file_name.clone()).await? {
                    self.db.delete_file_record(file_id).await.ok();
                }
                match tokio::fs::remove_file(entry.path()).await {
                    Ok(_) => {
                        removed += 1;
                        info!(
                            ">> CLEANER: removed expired trash file {}",
                            entry.path().display()
                        );
                    }
                    Err(e) => warn!(
                        ">> CLEANER: failed to remove {}: {}",
                        entry.path().display(),
                        e
                    ),
                }
            }
        }
        if removed > 0 {
            info!(">> CLEANER: cleaned {} expired trash file(s)", removed);
        }
        Ok(removed)
    }
}
