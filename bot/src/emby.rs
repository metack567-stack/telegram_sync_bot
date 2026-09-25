use anyhow::{Result, anyhow};
use core::fmt;
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::debug;

/// Emby 音乐库中的一首歌（Audio 条目）。
/// 字段名保持与 Emby `/Items` 接口 JSON 一致。
#[derive(Debug, Clone, Deserialize)]
#[allow(non_snake_case)]
pub struct EmbySong {
    pub Id: String,
    pub Name: String,
    #[serde(default)]
    pub Artists: Vec<String>,
    #[serde(default)]
    pub Album: Option<String>,
    pub Path: String,
}

/// 用户等待点播的 Emby 音乐库歌曲列表（/emby 查库点播用）。
#[derive(Debug, Clone)]
pub struct PendingEmby {
    pub songs: Vec<EmbySong>,
    pub created: Instant,
}

impl PendingEmby {
    pub fn expired(&self) -> bool {
        self.created.elapsed() > Duration::from_secs(60)
    }
}

/// 当前选中的 Emby 歌单（/playlist 命令创建/打开，/emby 搜索结果可一键加入）。
#[derive(Debug, Clone)]
pub struct PlaylistCtx {
    pub id: String,
    pub name: String,
}

/// Emby 歌单基本信息（列表选择用）。
#[derive(Debug, Clone)]
pub struct PlaylistInfo {
    pub id: String,
    pub name: String,
}

/// Emby 后端 HTTP 客户端（只读查询 + 库刷新 + 歌单管理，api_key 认证）。
/// 注意：Emby 返回的 Path 是 Emby 容器视角的路径；本 bot 的 MUSIC_DIR
/// 与 Emby 挂载同一宿主音乐库且同为 `/music` 时路径可直接使用。
pub struct EmbyClient {
    base: String,
    api_key: String,
    http: reqwest::Client,
    /// 歌单操作需要的用户 Id（懒获取并缓存；创建/加歌都用该用户，保证权限一致）
    user_id: Mutex<Option<String>>,
}

impl fmt::Debug for EmbyClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmbyClient")
            .field("base", &self.base)
            .field("api_key_set", &!self.api_key.is_empty())
            .finish()
    }
}

impl EmbyClient {
    pub fn new(base: String, api_key: String) -> Arc<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build emby http client");
        Arc::new(Self {
            base,
            api_key,
            http,
            user_id: Mutex::new(None),
        })
    }

    /// 按关键词搜索 Emby 音乐库中的 Audio 条目，返回原始命中列表（不过滤，供点播选择）。
    pub async fn search_songs(
        &self,
        keyword: &str,
        limit: usize,
    ) -> Result<Vec<EmbySong>> {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            return Ok(vec![]);
        }
        let limit = limit.clamp(1, 20);
        let resp = self
            .http
            .get(format!("{}/Items", self.base))
            .query(&[
                ("Recursive", "true"),
                ("IncludeItemTypes", "Audio"),
                ("SearchTerm", keyword),
                ("Fields", "Path,Artists,Album"),
                ("Limit", &limit.to_string()),
                ("api_key", &self.api_key),
            ])
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("emby query -> HTTP {status}: {text}"));
        }
        let json: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("emby query bad json: {e}: {text}"))?;
        let items = json
            .get("Items")
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default();
        let songs: Vec<EmbySong> = items
            .into_iter()
            .filter_map(|it| serde_json::from_value(it).ok())
            .collect();
        Ok(songs)
    }

    /// 按歌名（+歌手）在 Emby 音乐库中查询已有音频。
    /// 匹配规则（忽略大小写）：
    /// 1) 歌名完全相等 → 命中；
    /// 2) 歌名较长（>=3 字符）且 Emby 返回名包含关键词、且歌手匹配 → 命中（取第一条）。
    /// 查询失败返回 Err（调用方可降级到本地预查）；未命中返回 Ok(None)。
    pub async fn find_song(&self, name: &str, artist: Option<&str>) -> Result<Option<EmbySong>> {
        let keyword = name.trim();
        if keyword.is_empty() {
            return Ok(None);
        }
        let songs = self.search_songs(keyword, 20).await?;
        let name_l = keyword.to_lowercase();
        let name_len = name_l.chars().count();
        let artist_l = artist
            .map(|a| a.trim().to_lowercase())
            .filter(|a| !a.is_empty());

        let mut best: Option<EmbySong> = None;
        for song in songs {
            let n = song.Name.to_lowercase();
            let name_eq = n == name_l;
            let artist_ok = artist_l.as_ref().is_none_or(|a| {
                song.Artists.iter().any(|x| x.to_lowercase().contains(&**a))
            });
            if name_eq {
                return Ok(Some(song));
            }
            // 长关键词包含 + 歌手匹配才视为命中，避免“晴天”误配“晴天娃娃”
            if name_len >= 3 && n.contains(&name_l) && artist_ok && best.is_none() {
                debug!(name = %song.Name, path = %song.Path, "emby candidate");
                best = Some(song);
            }
        }
        Ok(best)
    }

    /// 触发 Emby 音乐库重新扫描（POST /Library/Refresh，异步 204）。
    /// 失败返回 Err（调用方 warn 即可，不阻断主流程）。
    pub async fn refresh_library(&self) -> Result<()> {
        let resp = self
            .http
            .post(format!("{}/Library/Refresh", self.base))
            .query(&[("api_key", &self.api_key)])
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("emby refresh -> HTTP {status}: {text}"));
        }
        Ok(())
    }

    /// 歌单操作用的用户 Id：懒获取（GET /Users 取第一个用户）并缓存。
    /// 创建歌单/加歌都使用该用户，保证与歌单所有者的权限一致。
    async fn user_id(&self) -> Result<String> {
        if let Some(uid) = self.user_id.lock().map(|g| g.clone()).unwrap_or(None) {
            return Ok(uid);
        }
        let resp = self
            .http
            .get(format!("{}/Users", self.base))
            .query(&[("api_key", &self.api_key)])
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("emby users -> HTTP {status}: {text}"));
        }
        let users: Vec<serde_json::Value> = serde_json::from_str(&text)
            .map_err(|e| anyhow!("emby users bad json: {e}"))?;
        let uid = users
            .first()
            .and_then(|u| u.get("Id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("no emby user found"))?;
        if let Ok(mut g) = self.user_id.lock() {
            *g = Some(uid.clone());
        }
        debug!(user_id = %uid, "emby user id cached");
        Ok(uid)
    }

    /// 查找同名歌单（忽略大小写），返回其 Id；未找到返回 Ok(None)。
    async fn find_playlist(&self, name: &str) -> Result<Option<String>> {
        let resp = self
            .http
            .get(format!("{}/Items", self.base))
            .query(&[
                ("Recursive", "true"),
                ("IncludeItemTypes", "Playlist"),
                ("SearchTerm", name),
                ("Limit", "10"),
                ("api_key", &self.api_key),
            ])
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("emby playlist query -> HTTP {status}: {text}"));
        }
        let json: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("emby playlist bad json: {e}"))?;
        let name_l = name.trim().to_lowercase();
        for it in json.get("Items").and_then(|i| i.as_array()).unwrap_or(&vec![]) {
            if it.get("Name").and_then(|v| v.as_str()).is_some_and(|n| n.to_lowercase() == name_l) {
                if let Some(id) = it.get("Id").and_then(|v| v.as_str()) {
                    return Ok(Some(id.to_string()));
                }
            }
        }
        Ok(None)
    }

    /// 创建歌单（POST /Playlists?Name=&UserId=），返回歌单 Id。
    pub async fn create_playlist(&self, name: &str) -> Result<String> {
        let uid = self.user_id().await?;
        let resp = self
            .http
            .post(format!("{}/Playlists", self.base))
            .query(&[
                ("Name", name),
                ("Ids", ""),
                ("UserId", &uid),
                ("api_key", &self.api_key),
            ])
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("emby create playlist -> HTTP {status}: {text}"));
        }
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("emby create playlist bad json: {e}: {text}"))?;
        v.get("Id")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("no playlist id in response: {text}"))
    }

    /// 查找或创建同名歌单，返回歌单 Id（查找优先，不存在则创建）。
    pub async fn find_or_create_playlist(&self, name: &str) -> Result<String> {
        if let Some(id) = self.find_playlist(name).await? {
            return Ok(id);
        }
        self.create_playlist(name).await
    }

    /// 把一首歌（Emby item Id）加入歌单。加歌必须带歌单所有者的 UserId。
    pub async fn add_to_playlist(&self, playlist_id: &str, song_id: &str) -> Result<()> {
        let uid = self.user_id().await?;
        let resp = self
            .http
            .post(format!("{}/Playlists/{}/Items", self.base, playlist_id))
            .query(&[
                ("Ids", song_id),
                ("UserId", &uid),
                ("api_key", &self.api_key),
            ])
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("emby add to playlist -> HTTP {status}: {text}"));
        }
        Ok(())
    }

    /// 列出歌单内的歌曲（GET /Playlists/{id}/Items）。
    pub async fn playlist_items(&self, playlist_id: &str) -> Result<Vec<EmbySong>> {
        let resp = self
            .http
            .get(format!("{}/Playlists/{}/Items", self.base, playlist_id))
            .query(&[
                ("Fields", "Path,Artists,Album"),
                ("Limit", "50"),
                ("api_key", &self.api_key),
            ])
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("emby playlist items -> HTTP {status}: {text}"));
        }
        let json: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("emby playlist items bad json: {e}: {text}"))?;
        let items = json
            .get("Items")
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default();
        let songs: Vec<EmbySong> = items
            .into_iter()
            .filter_map(|it| serde_json::from_value(it).ok())
            .collect();
        Ok(songs)
    }

    /// 列出 Emby 中全部播放列表（歌单），供 /playlist 无参数选择。
    pub async fn list_playlists(&self) -> Result<Vec<PlaylistInfo>> {
        let resp = self
            .http
            .get(format!("{}/Items", self.base))
            .query(&[
                ("Recursive", "true"),
                ("IncludeItemTypes", "Playlist"),
                ("Limit", "100"),
                ("api_key", &self.api_key),
            ])
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("emby playlists -> HTTP {status}: {text}"));
        }
        let json: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("emby playlists bad json: {e}"))?;
        let mut out = Vec::new();
        for it in json.get("Items").and_then(|i| i.as_array()).unwrap_or(&vec![]) {
            let id = it.get("Id").and_then(|v| v.as_str()).map(|s| s.to_string());
            let name = it.get("Name").and_then(|v| v.as_str()).map(|s| s.to_string());
            if let (Some(id), Some(name)) = (id, name) {
                out.push(PlaylistInfo { id, name });
            }
        }
        Ok(out)
    }

    /// 按 Id 查歌单基本信息（GET /Playlists/{id}），用于回调里反查名字。
    pub async fn get_playlist(&self, playlist_id: &str) -> Result<PlaylistInfo> {
        let resp = self
            .http
            .get(format!("{}/Playlists/{}", self.base, playlist_id))
            .query(&[("api_key", &self.api_key)])
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("emby playlist get -> HTTP {status}: {text}"));
        }
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("emby playlist get bad json: {e}: {text}"))?;
        let id = v
            .get("Id")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("no playlist id in response: {text}"))?;
        let name = v
            .get("Name")
            .and_then(|x| x.as_str())
            .unwrap_or("未知歌单")
            .to_string();
        Ok(PlaylistInfo { id, name })
    }
}
