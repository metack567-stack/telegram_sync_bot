use anyhow::{Result, anyhow};
use core::fmt;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;

/// Emby 音乐库中的一首歌（Audio 条目）。
/// 字段名保持与 Emby `/Items` 接口 JSON 一致。
#[derive(Debug, Clone, Deserialize)]
#[allow(non_snake_case)]
pub struct EmbySong {
    pub Name: String,
    #[serde(default)]
    pub Artists: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub Album: Option<String>,
    pub Path: String,
}

/// Emby 后端 HTTP 客户端（只读，api_key 认证）。
/// 用于下载前预查音乐库是否已有该歌，避免重复下载。
/// 注意：Emby 返回的 Path 是 Emby 容器视角的路径；本 bot 的 MUSIC_DIR
/// 与 Emby 挂载同一宿主音乐库且同为 `/music` 时路径可直接使用。
pub struct EmbyClient {
    base: String,
    api_key: String,
    http: reqwest::Client,
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
        })
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
        let resp = self
            .http
            .get(format!("{}/Items", self.base))
            .query(&[
                ("Recursive", "true"),
                ("IncludeItemTypes", "Audio"),
                ("SearchTerm", keyword),
                ("Fields", "Path,Artists,Album"),
                ("Limit", "20"),
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

        let name_l = keyword.to_lowercase();
        let name_len = name_l.chars().count();
        let artist_l = artist
            .map(|a| a.trim().to_lowercase())
            .filter(|a| !a.is_empty());

        let mut best: Option<EmbySong> = None;
        for it in items {
            let Ok(song) = serde_json::from_value::<EmbySong>(it) else {
                continue;
            };
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
}
