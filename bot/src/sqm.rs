use anyhow::{Result, anyhow, bail};
use core::fmt;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};
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

impl MusicRecord {
    /// 从 dataInfo 里宽松提取专辑封面 URL（兼容多种字段结构）；找不到返回 None。
    /// 通用字段（albumImgs/pic/imgUrl/cover/...）直接取；酷我(kw)源用
    /// `webAlbumpicShort`（CDN 相对路径）拼完整封面 URL（已验证 img1.kuwo.cn 可访问）。
    pub fn cover_url(&self) -> Option<String> {
        let d = self.dataInfo.as_ref()?;
        for key in [
            "albumImgs", "pic", "imgUrl", "cover", "albumPic", "albumImg", "image", "img",
        ] {
            let Some(v) = d.get(key) else {
                continue;
            };
            if let Some(s) = v.as_str() {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            } else if let Some(arr) = v.as_array() {
                if let Some(first) = arr.first() {
                    if let Some(s) = first.as_str() {
                        if !s.is_empty() {
                            return Some(s.to_string());
                        }
                    }
                    if let Some(obj) = first.as_object() {
                        for k2 in ["url", "img", "src"] {
                            if let Some(s) = obj.get(k2).and_then(|x| x.as_str()) {
                                if !s.is_empty() {
                                    return Some(s.to_string());
                                }
                            }
                        }
                    }
                }
            } else if let Some(obj) = v.as_object() {
                for k2 in ["url", "img", "src", "picUrl"] {
                    if let Some(s) = obj.get(k2).and_then(|x| x.as_str()) {
                        if !s.is_empty() {
                            return Some(s.to_string());
                        }
                    }
                }
            }
        }
        // 酷我(kw)：webAlbumpicShort 是 CDN 相对路径，拼 img1.kuwo.cn/star/albumcover/
        if let Some(s) = d.get("webAlbumpicShort").and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(format!("https://img1.kuwo.cn/star/albumcover/{}", s));
            }
        }
        None
    }
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
    pub downloadAlbumname: Option<String>,
    #[allow(dead_code)]
    pub downloadBrType: Option<String>,
}

/// 用户等待选歌/选音质的临时状态。
#[derive(Debug, Clone)]
pub struct PendingMusic {
    pub songs: Vec<MusicRecord>,
    pub created: Instant,
    /// 用户指定的音质偏好（如 "flac"/"320"），None 表示自动。
    pub pref: Option<String>,
    /// 已选中的歌曲下标（进入选音质阶段）；None 表示还在选歌阶段。
    pub chosen: Option<usize>,
    /// 搜索关键词（音质面板 🔙 返回时重建歌曲列表标题用）。
    pub keyword: String,
}

impl PendingMusic {
    pub fn expired(&self) -> bool {
        self.created.elapsed() > Duration::from_secs(60)
    }
}

/// 一次"下载试听"的操作状态：文件在临时区，等待用户 试听/入库/收藏/删除。
/// 有效期 10 分钟（听完歌再决定）；超时后按钮失效，文件留给后台定时清理。
#[derive(Debug, Clone)]
pub struct DownloadAct {
    /// 试听文件路径（临时区或音乐库文件；入库后临时文件已删除，试听按钮不再可用）
    pub tmp_path: PathBuf,
    /// 相对音乐库的路径（保留 歌手/专辑 子目录结构），入库时 copy 到 MUSIC_DIR 下
    pub rel_path: PathBuf,
    pub name: String,
    pub artist: String,
    pub album: Option<String>,
    pub created: Instant,
    /// 是否已入库（入库后按钮变为 ➕ 加入歌单）
    pub kept: bool,
    /// 是否临时区文件：true=删除安全（删临时文件）；false=音乐库文件（删除请走 /emby 面板）
    pub is_tmp: bool,
    /// Emby item id（音乐库命中时有值；删除已入库歌曲走 Emby DELETE 两步确认）
    pub emby_id: Option<String>,
    /// 音质/格式标识（面板文案展示用）
    pub br: String,
    /// 实际文件格式是否匹配所选音质（面板提示用）
    pub format_ok: bool,
}

impl DownloadAct {
    pub fn expired(&self) -> bool {
        self.created.elapsed() > Duration::from_secs(600)
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

/// 带用户偏好的码率选择：偏好（如 "flac"/"320"）命中则用之，否则回退到自动选择。
pub fn pick_br_type_with_pref(br_types: &[String], pref: Option<&str>) -> Option<String> {
    if let Some(pref) = pref {
        let p = pref.to_ascii_lowercase();
        if let Some(i) = br_types.iter().position(|b| b.to_ascii_lowercase().contains(&p)) {
            return Some(br_types[i].clone());
        }
    }
    pick_br_type(br_types)
}

/// 把 brType（如 "KW_FLAC_2000"）转成可读的按钮文案（如 "无损 FLAC 2000"）。
pub fn humanize_br(br: &str) -> String {
    let lower = br.to_ascii_lowercase();
    let format = if lower.contains("flac") {
        "无损 FLAC"
    } else if lower.contains("ape") {
        "无损 APE"
    } else if lower.contains("wav") {
        "无损 WAV"
    } else if lower.contains("m4a") || lower.contains("aac") {
        "AAC/M4A"
    } else if lower.contains("mp3") {
        "MP3"
    } else {
        br
    };
    let rate = if lower.contains("2000") {
        "2000"
    } else if lower.contains("320") {
        "320"
    } else if lower.contains("256") {
        "256"
    } else if lower.contains("128") {
        "128"
    } else {
        ""
    };
    if rate.is_empty() {
        format.to_string()
    } else {
        format!("{} {}", format, rate)
    }
}

/// 为一首歌生成音质选择的内联键盘：该歌可用码率各一个按钮，外加"自动"，
/// 底部一行 🔙 返回选歌 / ❌ 取消。
/// 回调数据格式：music:dl:<song_idx>:<brType>，brType 为 "auto" 表示自动选。
pub fn quality_keyboard(song_idx: usize, br_types: &[String]) -> InlineKeyboardMarkup {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for bt in br_types {
        if seen.contains(bt) {
            continue;
        }
        seen.push(bt.clone());
        let label = humanize_br(bt);
        rows.push(vec![InlineKeyboardButton::callback(
            label,
            format!("music:dl:{}:{}", song_idx, bt),
        )]);
    }
    rows.push(vec![InlineKeyboardButton::callback(
        "⚙️ 自动",
        format!("music:dl:{}:auto", song_idx),
    )]);
    rows.push(vec![
        InlineKeyboardButton::callback("🔙 返回选歌", "music:back"),
        InlineKeyboardButton::callback("❌ 取消", "music:cancel"),
    ]);
    InlineKeyboardMarkup::new(rows)
}

/// 找到的音频文件 + 是否与期望格式一致。
#[derive(Debug, Clone)]
pub struct FoundAudio {
    pub path: PathBuf,
    pub format_ok: bool,
}

/// 由 brType（如 "KW_FLAC_2000"）推导期望的音频扩展名；无法识别时返回 None。
pub fn ext_from_br(br: &str) -> Option<&'static str> {
    let lower = br.to_ascii_lowercase();
    if lower.contains("flac") {
        Some("flac")
    } else if lower.contains("ape") {
        Some("ape")
    } else if lower.contains("wav") {
        Some("wav")
    } else if lower.contains("m4a") || lower.contains("aac") {
        Some("m4a")
    } else if lower.contains("mp3") {
        Some("mp3")
    } else {
        None
    }
}

/// 在音乐目录下找刚下载（或已存在）的音频文件。
/// sqmusic 对库中已存在的歌会跳过下载（任务仍返回 success），所以先按任务信息
/// （歌名 + 歌手）在音乐库全局匹配已有文件，命中即返回；否则回退到 5 分钟内新增文件。
/// `prefer_ext`（如 "flac"）指定时优先返回该格式；找不到同格式才回退其他格式
/// （format_ok=false，调用方应提示用户）。
pub async fn find_latest_audio(
    music_dir: &PathBuf,
    task: &TaskRecord,
    prefer_ext: Option<&str>,
) -> Result<FoundAudio> {
    if !music_dir.is_dir() {
        bail!("音乐目录不存在：{}", music_dir.display());
    }
    if let Some(f) = find_by_task(music_dir, task, prefer_ext) {
        return Ok(f);
    }
    let now = SystemTime::now();
    let window = Duration::from_secs(300);
    let mut best_pref: Option<(SystemTime, PathBuf)> = None;
    let mut best_fallback: Option<(SystemTime, PathBuf)> = None;
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
        if !now.duration_since(modified).map(|d| d <= window).unwrap_or(false) {
            continue;
        }
        let ext_ok = prefer_ext.is_none_or(|e| name.ends_with(e));
        let slot = if ext_ok {
            &mut best_pref
        } else {
            &mut best_fallback
        };
        if slot.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
            *slot = Some((modified, entry.into_path()));
        }
    }
    if let Some((_, p)) = best_pref {
        Ok(FoundAudio { path: p, format_ok: true })
    } else if let Some((_, p)) = best_fallback {
        Ok(FoundAudio { path: p, format_ok: false })
    } else {
        bail!("音乐目录里未找到歌曲文件（歌名+歌手均未匹配到）")
    }
}

/// 按歌名/歌手在音乐库中匹配已有音频文件（下载前预查用）。找不到返回 None。
/// `prefer_ext` 指定时优先返回该格式，否则回退其他格式（format_ok=false）。
pub fn find_in_library(
    music_dir: &PathBuf,
    song: &MusicRecord,
    prefer_ext: Option<&str>,
) -> Option<FoundAudio> {
    let song_l = song.name.trim().to_lowercase();
    if song_l.is_empty() {
        return None;
    }
    let artist_l = if song.artistName.is_empty() {
        None
    } else {
        Some(song.artistName.join(" ").to_lowercase())
    };
    match_in_library(music_dir, &song_l, artist_l.as_deref(), prefer_ext)
}

/// 按任务记录的歌名/歌手在音乐库中匹配已有音频文件。找不到返回 None。
fn find_by_task(
    music_dir: &PathBuf,
    task: &TaskRecord,
    prefer_ext: Option<&str>,
) -> Option<FoundAudio> {
    let song = task
        .downloadMusicname
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())?;
    let artist = task
        .downloadArtistname
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    match_in_library(
        music_dir,
        &song.to_lowercase(),
        artist.map(|a| a.to_lowercase()).as_deref(),
        prefer_ext,
    )
}

/// 在音乐库中按歌名/歌手匹配已有音频文件的核心逻辑。
/// 多级匹配，容忍文件名差异：
/// 1) 文件名分词（按 `-`/`_`/空格/括号等拆分）后存在一段与歌名完全相等 → 命中；
/// 2) 文件名同时包含歌名和歌手 → 命中；
/// 3) 歌名较长（>=3 字符）且文件名包含歌名 → 命中（短歌名如“晴天”只允许前两级，避免误配“晴天娃娃”）。
/// `prefer_ext` 指定时优先返回该格式，否则回退其他格式（format_ok=false）。
fn match_in_library(
    music_dir: &PathBuf,
    song_l: &str,
    artist_l: Option<&str>,
    prefer_ext: Option<&str>,
) -> Option<FoundAudio> {
    let song_len = song_l.chars().count();
    let mut best_pref: Option<(SystemTime, PathBuf)> = None;
    let mut best_fallback: Option<(SystemTime, PathBuf)> = None;
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
        let stem = strip_audio_ext(&name);
        let tokens: Vec<&str> = stem
            .split(['-', '_', ' ', '.', '(', ')', '【', '】', '[', ']'])
            .filter(|t| !t.is_empty())
            .collect();
        let token_exact = tokens.iter().any(|t| *t == song_l);
        let contains_song = stem.contains(song_l);
        let contains_artist = artist_l.is_some_and(|a| stem.contains(a));
        let matched = token_exact
            || (contains_song && contains_artist)
            || (contains_song && song_len >= 3);
        if !matched {
            continue;
        }
        let modified = entry.metadata().ok()?.modified().ok()?;
        let ext_ok = prefer_ext.is_none_or(|e| name.ends_with(e));
        let slot = if ext_ok {
            &mut best_pref
        } else {
            &mut best_fallback
        };
        if slot.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
            *slot = Some((modified, entry.into_path()));
        }
    }
    if let Some((_, p)) = best_pref {
        Some(FoundAudio { path: p, format_ok: true })
    } else if let Some((_, p)) = best_fallback {
        Some(FoundAudio { path: p, format_ok: false })
    } else {
        None
    }
}

/// 去掉文件名末尾的音频扩展名。
fn strip_audio_ext(name: &str) -> &str {
    const EXTS: [&str; 8] = ["mp3", "flac", "m4a", "ape", "wav", "ogg", "aac", "opus"];
    for e in EXTS {
        if let Some(stem) = name.strip_suffix(e) {
            return stem;
        }
    }
    name
}

fn is_audio_ext(name: &str) -> bool {
    const EXTS: [&str; 8] = ["mp3", "flac", "m4a", "ape", "wav", "ogg", "aac", "opus"];
    EXTS.iter().any(|e| name.ends_with(e))
}

/// 清理临时试听区：删除超过 `older_than_secs` 未入库的文件；若剩余仍超过 `keep`
/// 个，再删除最旧的。返回删除数量。目录不存在时直接返回 0（不视为错误）。
pub async fn cleanup_tmp_dir(dir: &PathBuf, older_than_secs: u64, keep: usize) -> Result<u32> {
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in walkdir::WalkDir::new(dir)
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
        if let Ok(meta) = entry.metadata()
            && let Ok(t) = meta.modified()
        {
            files.push((t, entry.into_path()));
        }
    }
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(older_than_secs))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut removed = 0u32;
    let mut keep_list: Vec<(SystemTime, PathBuf)> = Vec::new();
    for (t, p) in files {
        if t < cutoff {
            match tokio::fs::remove_file(&p).await {
                Ok(_) => {
                    removed += 1;
                    info!(">> CLEANER: tmp music removed {}", p.display());
                }
                Err(e) => warn!(">> CLEANER: tmp music remove failed {}: {}", p.display(), e),
            }
        } else {
            keep_list.push((t, p));
        }
    }
    if keep_list.len() > keep {
        // 按修改时间由旧到新排序，超出 keep 的删掉
        keep_list.sort_by(|a, b| a.0.cmp(&b.0));
        let overflow = keep_list.len().saturating_sub(keep);
        for (_, p) in keep_list.into_iter().take(overflow) {
            match tokio::fs::remove_file(&p).await {
                Ok(_) => {
                    removed += 1;
                    info!(">> CLEANER: tmp music evicted {}", p.display());
                }
                Err(e) => warn!(">> CLEANER: tmp music evict failed {}: {}", p.display(), e),
            }
        }
    }
    Ok(removed)
}
