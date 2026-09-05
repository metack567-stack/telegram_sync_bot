use core::fmt;
use parking_lot::RwLock;
use std::ops::Deref;
use std::sync::atomic::AtomicBool;
use std::{collections::HashSet, path::PathBuf, sync::Arc};
use teloxide::types::UserId;

#[derive(Debug, Clone)]
pub struct Context {
    pub inner: Arc<ContextInner>,
}

#[derive(Debug)]
pub struct ContextInner {
    pub local_server: bool,
    pub container_manager: Option<String>,
    pub container_id: Option<String>,

    pub bypasskey: RwLock<String>,
    pub bypass_users: Option<HashSet<UserId>>,

    // channel only: score >= fav_score_limit will be fav
    pub fav_score_limit: i32,
    // channel only: score < dislike_score_limit will be deleted
    pub dislike_score_limit: i32,

    pub data_dir: PathBuf,
    // where data.db and bypass.key live (env DB_DIR)
    pub db_dir: PathBuf,
    // root of the local telegram-bot-api cache, seen from this container (env SERVER_CACHE_DIR)
    pub server_cache_dir: PathBuf,

    pub hard_link: AtomicBool,
}

impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("localserver", &self.local_server)
            .field("container_manager", &self.container_manager)
            .field("container_id", &self.container_id)
            .field("bypasskey", &self.bypasskey.read())
            .field("bypass_users", &self.bypass_users)
            .field("fav", &format!("score >= {}", self.fav_score_limit))
            .field("delete", &format!("score < {}", self.dislike_score_limit))
            .field("output_dir", &self.data_dir.canonicalize().ok())
            .field("db_dir", &self.db_dir.canonicalize().ok())
            .field("server_cache_dir", &self.server_cache_dir.canonicalize().ok())
            .finish()
    }
}

impl Deref for Context {
    type Target = ContextInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
