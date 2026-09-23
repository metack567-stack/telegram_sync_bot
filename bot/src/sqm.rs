use anyhow::{Result, anyhow, bail};
use core::fmt;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tracing::{info, warn};

/// 搜索结果中的一首歌（对应 sqmusic `/api/music/searchSong` 返回的 records[]）。
/// 字段名保持与接口 JSON 一致，便于直接作为下载请求体。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[allow(non_snake_case)]
pub struct MusicRecord {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub artistName: Vec<String>,
    #[serde(default)]
    pub albumName: Option<String>,
    #[serde(default)]
    pub brTypes: Vec<String>,
    #[serde(default)]
    pub dataInfo: Option<serde_json::Value>,
    #[serde(default)]
    pub plugName: String,
}

/// 下载任务记录（对应 `/api/task/list` 返回的 records[]）。
/// 字段名保持与接口 JSON 一致（camelCase 契约）。
#[derive(Debug, Clone, Deserialize)]
#[allow(non_snake_case)]
pub struct TaskRecord {
    #[allow(dead_code)]
    pub id: Option<i64>,
    pub downloadGid: Option<String>,
    pub downloadStatus: Option<String>,
    pub downloadMsg: Option<String>,
    #[allow(dead_code)]
    pub downloadFile: Option<String>,
    pub downloadMusicname: Option<String>,
    pub downloadArtistname: Option<String>,
    #[allow(dead_code)]
    pub downloadAlbumname: Option<String>,
    #[allow(dead_code)]
    pub downloadBrType: Option<String>,
}

/// 用户等待选歌的临时状态。
#[derive(Debug, Clone)]
pub struct PendingMusic {
    pub songs: Vec<MusicRecord>,
    pub created: Instant,
}

impl PendingMusic {
    pub fn expired(&self) -> bool {
        self.created.elapsed() > Duration::from_secs(60)
    }
}

/// sqmusic 后端 HTTP 客户端。Sa-Token 鉴权：登录后把 token 放 `sqmusic` 请求头。
pub struct SqmusicClient {
    base: String,
    user: String,
    pass: String,
    http: reqwest::Client,
    token: RwLock<Option<String>>,
}

impl fmt::Debug for SqmusicClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqmusicClient")
            .field("base", &self.base)
            .field("user", &self.user)
            .field("token_cached", &self.token.read().is_some())
            .finish()
    }
}

impl SqmusicClient {
    pub fn new(base: String, user: String, pass: String) -> Arc<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build sqmusic http client");
        Arc::new(Self {
            base,
            user,
            pass,
            http,
            token: RwLock::new(None),
        })
    }

    /// 确保已有有效 token（懒登录 + 失效后重登）。
    async fn ensure_token(&self) -> Result<String> {
        if let Some(t) = self.token.read().as_ref() {
            return Ok(t.clone());
        }
        self.login().await
    }

    async fn login(&self) -> Result<String> {
        let body = serde_json::json!({
            "username": self.user,
            "password": self.pass,
            "device": "web",
        });
        let resp = self
            .http
            .post(format!("{}/api/config/login", self.base))
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        let json: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        let token = json
            .get("data")
            .and_then(|d| d.get("tokenValue"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("sqmusic login failed: HTTP {status}: {text}"))?;
        info!(">> SQMUSIC: logged in, token cached");
        *self.token.write() = Some(token.to_string());
        Ok(token.to_string())
    }

    /// 统一请求封装：带 token 头；接口返回 code!=200 或 HTTP 非 2xx 时重登重试一次。
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<serde_json::Value>) -> Result<serde_json::Value> {
        let attempt = |client: &SqmusicClient, token: &str| {
            let url = format!("{}{}", client.base, path);
            let mut req = client
                .http
                .request(method.clone(), url)
                .header("sqmusic", token)
                .header("Content-Type", "application/json");
            if let Some(b) = body.clone() {
                req = req.json(&b);
            }
            req.send()
        };
        let mut token = self.ensure_token().await?;
        let mut resp = attempt(self, &token).await?;
        let mut status = resp.status();
        let mut text = resp.text().await?;
        if !status.is_success() {
            warn!(">> SQMUSIC: {} -> HTTP {status}, re-login and retry once", path);
            token = self.login().await?;
            resp = attempt(self, &token).await?;
            status = resp.status();
            text = resp.text().await?;
        }
        if !status.is_success() {
            bail!("sqmusic {} -> HTTP {status}: {text}", path);
        }
        let json: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("sqmusic {} bad json: {e}: {text}", path))?;
        let code = json.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 200 {
            let msg = json
                .get("msg")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            bail!("sqmusic {} -> code {code}: {msg}", path);
        }
        Ok(json)
    }

    /// 搜索歌曲，返回最多 `limit` 条。
    pub async fn search(&self, plug: &str, keyword: &str, limit: usize) -> Result<Vec<MusicRecord>> {
        let limit = limit.clamp(1, 20);
        let resp = self
            .http
            .get(format!("{}/api/music/searchSong", self.base))
            .header("sqmusic", self.ensure_token().await?)
            .query(&[
                ("plugName", plug),
                ("keyword", keyword),
                ("pageSize", &limit.to_string()),
                ("pageIndex", "1"),
            ])
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            bail!("sqmusic search -> HTTP {status}: {text}");
        }
        let json: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("sqmusic search bad json: {e}: {text}"))?;
        let code = json.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 200 {
            let msg = json.get("msg").and_then(|m| m.as_str()).unwrap_or("unknown error");
            bail!("sqmusic search -> code {code}: {msg}");
        }
        let records = json
            .get("data")
            .and_then(|d| d.get("records"))
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        let songs: Vec<MusicRecord> = records
            .into_iter()
            .filter_map(|r| serde_json::from_value(r).ok())
            .collect();
        Ok(songs)
    }

    /// 提交单曲下载任务。
    pub async fn download_song(&self, record: &MusicRecord, br_type: &str) -> Result<()> {
        let mut payload = serde_json::to_value(record)
            .map_err(|e| anyhow!("failed to serialize record: {e}"))?;
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("brType".to_string(), serde_json::Value::String(br_type.to_string()));
        }
        self.call(reqwest::Method::POST, "/api/download/downloadSong", Some(payload))
            .await?;
        Ok(())
    }

    /// 拉取最近下载任务。
    pub async fn tasks(&self) -> Result<Vec<TaskRecord>> {
        let json = self
            .call(
                reqwest::Method::POST,
                "/api/task/list",
                Some(serde_json::json!({"pageIndex": 1, "pageSize": 30})),
            )
            .await?;
        let records = json
            .get("data")
            .and_then(|d| d.get("records"))
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        let tasks: Vec<TaskRecord> = records
            .into_iter()
            .filter_map(|r| serde_json::from_value(r).ok())
            .collect();
        Ok(tasks)
    }

    /// 等待某个搜索 id（downloadGid）的下载任务结束（success/error），最多 `timeout` 秒。
    pub async fn wait_task(&self, gid: &str, timeout_secs: u64) -> Result<TaskRecord> {
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            if let Some(task) = self.tasks().await?.into_iter().find(|t| t.downloadGid.as_deref() == Some(gid)) {
                match task.downloadStatus.as_deref() {
                    Some("success") => return Ok(task),
                    Some("error") => bail!(
                        "{}",
                        task.downloadMsg
                            .clone()
                            .unwrap_or_else(|| "下载失败（未知原因）".to_string())
                    ),
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                bail!("下载超时（{}s），请到 sqmusic 页面查看任务状态", timeout_secs);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

/// 从候选码率里挑一个：优先无损，其次 320，再其次 128/默认第一个。
pub fn pick_br_type(br_types: &[String]) -> Option<String> {
    let lower = br_types.iter().map(|s| s.to_ascii_lowercase()).collect::<Vec<_>>();
    for pref in ["flac", "ape", "wav", "m4a"] {
        if let Some(i) = lower.iter().position(|s| s.contains(pref)) {
            return Some(br_types[i].clone());
        }
    }
    for pref in ["_320", "320"] {
        if let Some(i) = lower.iter().position(|s| s.contains(pref)) {
            return Some(br_types[i].clone());
        }
    }
    br_types.first().cloned()
}

/// 在音乐目录下找最近 5 分钟内新增的音频文件（取最新）。
/// 目录不存在或没有新文件时返回错误。
pub async fn find_latest_audio(music_dir: &PathBuf) -> Result<PathBuf> {
    if !music_dir.is_dir() {
        bail!("音乐目录不存在：{}", music_dir.display());
    }
    let now = SystemTime::now();
    let window = Duration::from_secs(300);
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in walkdir::WalkDir::new(music_dir)
        .max_depth(4)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if !is_audio_ext(&name) {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = match meta.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if now.duration_since(modified).map(|d| d <= window).unwrap_or(false)
            && best.as_ref().map(|(t, _)| modified > *t).unwrap_or(true)
        {
            best = Some((modified, entry.into_path()));
        }
    }
    best.map(|(_, p)| p)
        .ok_or_else(|| anyhow!("音乐目录里未找到刚下载的音频文件（5 分钟内）"))
}

fn is_audio_ext(name: &str) -> bool {
    const EXTS: [&str; 8] = ["mp3", "flac", "m4a", "ape", "wav", "ogg", "aac", "opus"];
    EXTS.iter().any(|e| name.ends_with(e))
}
