use super::state::TransportState;
use crate::context::Context;
use crate::utils::cp_from_container;
use anyhow::Result;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;
use teloxide::Bot;
use teloxide::net::Download as _;
use teloxide::prelude::{Request as _, Requester as _};
use tokio::fs;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct TransportHandle {
    state: Arc<RwLock<TransportState>>,
    cancel: CancellationToken,
}

impl TransportHandle {
    fn new() -> Self {
        TransportHandle {
            state: Arc::new(RwLock::new(TransportState::default())),
            cancel: CancellationToken::new(),
        }
    }

    pub fn get_state(&self) -> TransportState {
        *self.state.read()
    }

    fn set_state(&self, state: TransportState) {
        *self.state.write() = state;
    }

    pub(super) fn cancel(&self) {
        self.cancel.cancel();
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub async fn result(&self) -> TransportState {
        self.cancel.cancelled().await;
        self.get_state()
    }
}

pub type FileId = String;
pub type FileName = String;

#[derive(Debug)]
pub struct Downloader {
    downloads: RwLock<HashMap<FileId, TransportHandle>>,
    tx: Sender<Message>,
    jh: RwLock<Option<JoinHandle<()>>>,
}

enum Message {
    Add(FileId, FileName, TransportHandle),
    Cancel(FileId),
    Shutdown,
}

impl Downloader {
    pub(super) fn new(bot: Bot, context: Context) -> Self {
        let (tx, rx) = channel::<Message>();
        let jh = std::thread::spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let cancels = Arc::new(RwLock::new(HashMap::new()));
                    // limit how many files are downloaded at the same time
                    // (env DOWNLOAD_CONCURRENCY, default 3, range 1..=10)
                    let concurrency = std::env::var("DOWNLOAD_CONCURRENCY")
                        .ok()
                        .and_then(|v| v.parse::<usize>().ok())
                        .filter(|n| (1..=10).contains(n))
                        .unwrap_or(3);
                    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));
                    while let Ok(msg) = rx.recv() {
                        match msg {
                            Message::Add(file_id, file_name, handle) => {
                                let bot = bot.clone();
                                let context = context.clone();
                                if let Some(old) = cancels.write().insert(file_id.clone(), handle.cancel.clone()) {
                                    info!(">> DOWNLOADER: dumplicated, cancel old task {}", file_id);
                                    old.cancel();
                                };
                                async fn download(
                                    bot: Bot,
                                    file_id: FileId,
                                    file_name: FileName,
                                    context: Context,
                                    handle: TransportHandle,
                                    sem: Arc<tokio::sync::Semaphore>
                                ) -> Result<()> {
                                    // wait for a free slot before starting the download
                                    let _permit = sem.acquire().await?;
                                    info!(">> DOWNLOADER: start task {}", file_id);
                                    handle.set_state(TransportState::Downloading);
                                    let save_path = context.data_dir.join(file_name);
                                    // retry get_file for at most ~60s, then give up instead of looping forever
                                    let mut attempts = 0u32;
                                    let server_path = loop {
                                        if let Ok(f) = bot.get_file(&file_id).send().await {
                                            // check free space before downloading, fail
                                            // fast instead of filling the disk
                                            if f.size > 0 {
                                                let avail =
                                                    crate::utils::available_bytes(&context.data_dir)?;
                                                if f.size as u64 > avail {
                                                    return Err(anyhow::anyhow!(
                                                        "not enough disk space: need {} bytes, only {} available for {}",
                                                        f.size,
                                                        avail,
                                                        file_id
                                                    ));
                                                }
                                            }
                                            break PathBuf::from(f.path);
                                        }
                                        attempts += 1;
                                        if attempts >= 20 {
                                            return Err(anyhow::anyhow!(
                                                "failed to get file path for {} after {} attempts",
                                                file_id,
                                                attempts
                                            ));
                                        }
                                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                    };
                                    // path of the file inside the local telegram-bot-api
                                    // cache: it may already be absolute (server container
                                    // view) or relative to the server cache root
                                    let server_abs = if server_path.is_absolute() {
                                        server_path.clone()
                                    } else {
                                        context.server_cache_dir.join(&server_path)
                                    };
                                    // download file
                                    match context.local_server {
                                        false => {
                                            let mut file = fs::File::create(&save_path).await?;
                                            bot.download_file(
                                                server_path.to_string_lossy().as_ref(),
                                                &mut file,
                                            )
                                            .await?;
                                        }
                                        true => match &context.container_manager {
                                            Some(container_manager) => {
                                                cp_from_container(
                                                    container_manager,
                                                    context.container_id.as_ref().unwrap(),
                                                    &server_path,
                                                    &save_path,
                                                )
                                                .await?;
                                            }
                                            None => {
                                                if context.hard_link.load(Ordering::Relaxed) {
                                                    if fs::hard_link(&server_abs,  &save_path).await.is_err() {
                                                        context.hard_link.store(false, Ordering::Relaxed);
                                                        info!(">> DOWNLOADER: file in local server cannot hard link to output, use copy instead");
                                                        fs::copy(&server_abs, &save_path).await?;
                                                    }
                                                } else {
                                                    fs::copy(&server_abs, &save_path).await?;
                                                }
                                            }
                                        },
                                    }
                                    info!(">> DOWNLOADER: finish task {}", file_id);
                                    if context.local_server {
                                        // remove the file from the local telegram-bot-api cache,
                                        // the bot already holds a hard link or a copy
                                        if let Err(e) = fs::remove_file(&server_abs).await {
                                            warn!(
                                                ">> DOWNLOADER: failed to remove server cache {}: {}",
                                                server_path.display(),
                                                e
                                            );
                                        }
                                    }
                                    Ok(())
                                }
                                async fn download_with_retry(
                                    bot: Bot,
                                    file_id: FileId,
                                    file_name: FileName,
                                    context: Context,
                                    handle: TransportHandle,
                                    sem: Arc<tokio::sync::Semaphore>,
                                ) -> Result<()> {
                                    let mut attempt = 0u32;
                                    loop {
                                        match download(
                                            bot.clone(),
                                            file_id.clone(),
                                            file_name.clone(),
                                            context.clone(),
                                            handle.clone(),
                                            sem.clone(),
                                        )
                                        .await
                                        {
                                            Ok(()) => return Ok(()),
                                            Err(e) => {
                                                attempt += 1;
                                                if attempt >= 3 {
                                                    return Err(e);
                                                }
                                                warn!(
                                                    ">> DOWNLOADER: download failed, retry {}: {}",
                                                    attempt + 1,
                                                    e
                                                );
                                                tokio::time::sleep(std::time::Duration::from_secs(
                                                    5 * attempt as u64,
                                                ))
                                                .await;
                                                if handle.cancel.is_cancelled() {
                                                    return Err(anyhow::anyhow!("cancelled"));
                                                }
                                            }
                                        }
                                    }
                                }
                                let cancels_c = cancels.clone();
                                let sem_c = sem.clone();
                                tokio::spawn(async move {
                                    tokio::select! {
                                        res = download_with_retry(bot, file_id.clone(), file_name.clone(), context.clone(), handle.clone(), sem_c) => {
                                            match res {
                                                Ok(_) => handle.set_state(TransportState::Completed),
                                                Err(e) => {
                                                    warn!(">> DOWNLOADER {}", e);
                                                    handle.set_state(TransportState::Failed);
                                                },
                                            }
                                            handle.cancel(); // when downloading, await cancel.cancelled() avoiding loop checking
                                        },
                                        _ = handle.cancel.cancelled() => {
                                            info!(">> DOWNLOADER: task cancelled {}", file_id);
                                            handle.set_state(TransportState::Cancelled);
                                            // remove the partial file a cancelled download may have left behind
                                            let _ = fs::remove_file(context.data_dir.join(&file_name)).await;
                                        },
                                    };
                                    cancels_c.write().remove(&file_id);
                                });
                            }
                            Message::Cancel(k) => {
                                if let Some(cancel) = cancels.write().remove(&k) {
                                    cancel.cancel();
                                }
                            }
                            Message::Shutdown => {
                                info!(">> DOWNLOADER: shutdown");
                                for (_, cancel) in cancels.read().iter() {
                                    cancel.cancel();
                                }
                                break;
                            }
                        }
                    }
                })
        });
        let tx_c = tx.clone();
        let _listener = tokio::spawn(async move {
            tokio::signal::ctrl_c().await?;
            tx_c.send(Message::Shutdown).ok();
            Ok::<_, anyhow::Error>(())
        });
        Downloader {
            downloads: RwLock::new(HashMap::new()),
            tx,
            jh: RwLock::new(Some(jh)),
        }
    }

    #[must_use = "Caller should refresh db state with this handle"]
    pub(super) fn add(&self, file_id: FileId, file_name: FileName) -> TransportHandle {
        let read = self.downloads.read();
        match read.get(&file_id).cloned() {
            Some(handle)
                if matches!(
                    handle.get_state(),
                    TransportState::Downloading
                        | TransportState::Pending
                        | TransportState::Completed
                ) =>
            {
                handle
            }
            _ => {
                drop(read);
                let handle = TransportHandle::new();
                if let Err(e) = self
                    .tx
                    .send(Message::Add(file_id.clone(), file_name, handle.clone()))
                {
                    warn!(">> DOWNLOADER: failed to submit task: {}", e);
                }
                if let Some(old_handle) = self.downloads.write().insert(file_id, handle.clone()) {
                    old_handle.cancel();
                }
                handle
            }
        }
    }

    pub(super) fn cancel(&self, file_id: FileId) {
        if let Err(e) = self.tx.send(Message::Cancel(file_id)) {
            warn!(">> DOWNLOADER: failed to send cancel: {}", e);
        }
    }

    /// remove a finished handle from the map, only if it is still the same handle
    pub(super) fn remove(&self, file_id: &str, handle: &TransportHandle) {
        let mut w = self.downloads.write();
        if let Some(h) = w.get(file_id) {
            if std::sync::Arc::ptr_eq(&h.state, &handle.state) {
                w.remove(file_id);
            }
        }
    }

    fn shutdown(&self) {
        self.tx.send(Message::Shutdown).ok();
        if let Some(jh) = self.jh.write().take() {
            jh.join().unwrap();
        }
    }
}

impl Drop for Downloader {
    fn drop(&mut self) {
        self.shutdown();
    }
}
