use super::{
    MyDialogue,
    command::cmd_handler,
    utils::{TryMultipleTimes, set_emoji},
};
use crate::{
    context::Context,
    emby::{EmbyClient, EmbySong},
    sqm::{DownloadAct, MusicRecord, SqmusicClient},
    storage::{ChatState, FileState, MyStorage, TransportState},
};
use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use teloxide::{
    Bot,
    dispatching::{UpdateFilterExt as _, UpdateHandler},
    prelude::Requester as _,
    requests::HasPayload as _,
    types::{
        ChatId, InlineKeyboardButton, InlineKeyboardMarkup, InputFile, MediaKind, MediaText,
        Message, MessageKind, Update,
    },
};
use tracing::{info, instrument, warn};

pub fn msg_handler() -> UpdateHandler<anyhow::Error> {
    Update::filter_message()
        .branch(cmd_handler())
        .endpoint(handle)
}

pub fn channel_post_handler() -> UpdateHandler<anyhow::Error> {
    Update::filter_channel_post()
        .branch(cmd_handler())
        .endpoint(handle)
}

#[instrument(level = "debug", skip_all, fields(chat_id=%msg.chat.id, msg_id=%msg.id))]
async fn handle(bot: Bot, dialogue: MyDialogue, msg: Message, storage: MyStorage, ctx: Context) -> Result<()> {
    let (chat_id, mut msg_id) = (msg.chat.id, msg.id);
    let chat_state = storage.get_chat_state(chat_id).await?;
    if chat_state == ChatState::Paused {
        // silently ignore while paused, avoid replying to every message
        dialogue.exit().await?;
        return Ok(());
    }
    // sqmusic: user picks a song number right after /music
    if ctx.sqmusic.is_some()
        && ctx.music_dir.is_some()
        && let MessageKind::Common(common) = &msg.kind
        && let MediaKind::Text(MediaText { text, .. }) = &common.media_kind
    {
        let pending = ctx.music_pending.lock().get(&chat_id).cloned();
        if let Some(pending) = pending {
            if pending.expired() {
                ctx.music_pending.lock().remove(&chat_id);
            } else if let Ok(n) = text.trim().parse::<usize>() {
                let len = pending.songs.len();
                if (1..=len).contains(&n) {
                    let song = pending.songs[n - 1].clone();
                    let artist = if song.artistName.is_empty() {
                        "未知歌手".to_string()
                    } else {
                        song.artistName.join("/")
                    };
                    // 进入"选音质"阶段：记录选中歌曲，发可点击的音质按钮（点击后才下载）
                    let mut p = pending.clone();
                    p.chosen = Some(n - 1);
                    ctx.music_pending.lock().insert(chat_id, p);
                    let kb = crate::sqm::quality_keyboard(n - 1, &song.brTypes);
                    let mut req = bot.send_message(
                        chat_id,
                        format!("🎚️ 请选择「{} - {}」的音质：", song.name, artist),
                    );
                    req.payload_mut().reply_markup =
                        Some(teloxide::types::ReplyMarkup::InlineKeyboard(kb));
                    req.await?;
                    return Ok(());
                }
                bot.send_message(chat_id, format!("请输入 1-{} 选择歌曲，或重新 /music 搜索", len))
                    .await?;
                return Ok(());
            }
        }
    }
    if let MessageKind::Common(common_msg) = msg.kind
        && let Some((file_id, file_name)) = match common_msg.media_kind {
            MediaKind::Document(document) => {
                // gif will be handled here too
                let file_id = document.document.file.id;
                let file_name = document.document.file_name.unwrap_or(file_id.clone());
                if document.media_group_id.is_some() {
                    // break up the group
                    let old = msg_id;
                    msg_id = (|| bot.send_document(chat_id, InputFile::file_id(&file_id)))
                        .try_multiple_times(3)
                        .await?
                        .id;
                    if let Err(e) = (|| bot.delete_message(chat_id, old))
                        .try_multiple_times(3)
                        .await
                    {
                        warn!(">> BOT: failed to delete original media-group message {}: {}", old, e);
                    }
                }
                Some((file_id, file_name))
            }
            MediaKind::Video(video) => {
                let file_id = video.video.file.id;
                let file_name = video.video.file_name.unwrap_or(format!(
                    "{}.{}",
                    file_id.clone(),
                    video
                        .video
                        .mime_type
                        .and_then(|m| m.suffix().map(|n| n.as_str().to_owned()))
                        .unwrap_or("mp4".to_string())
                ));
                if video.media_group_id.is_some() {
                    // break up the group
                    let old = msg_id;
                    msg_id = (|| bot.send_video(chat_id, InputFile::file_id(&file_id)))
                        .try_multiple_times(3)
                        .await?
                        .id;
                    if let Err(e) = (|| bot.delete_message(chat_id, old))
                        .try_multiple_times(3)
                        .await
                    {
                        warn!(">> BOT: failed to delete original media-group message {}: {}", old, e);
                    }
                }
                Some((file_id, file_name))
            }
            MediaKind::Audio(audio) => {
                let file_id = audio.audio.file.id;
                let file_name = audio.audio.file_name.unwrap_or(format!(
                    "{}.{}",
                    file_id.clone(),
                    audio
                        .audio
                        .mime_type
                        .and_then(|m| m.suffix().map(|n| n.as_str().to_owned()))
                        .unwrap_or("mp3".to_string())
                ));
                if audio.media_group_id.is_some() {
                    // break up the group
                    let old = msg_id;
                    msg_id = (|| bot.send_audio(chat_id, InputFile::file_id(&file_id)))
                        .try_multiple_times(3)
                        .await?
                        .id;
                    if let Err(e) = (|| bot.delete_message(chat_id, old))
                        .try_multiple_times(3)
                        .await
                    {
                        warn!(">> BOT: failed to delete original media-group message {}: {}", old, e);
                    }
                }
                Some((file_id, file_name))
            }
            MediaKind::Photo(photo) => {
                let file = photo.photo.into_iter().max_by_key(|p| p.height).unwrap();
                let file_id = file.file.id;
                // prefer the caption as a readable file name; fall back to the file id
                let file_name = match photo.caption.as_deref() {
                    Some(c) if !c.trim().is_empty() => {
                        let cleaned: String = c
                            .chars()
                            .map(|ch| {
                                if ch.is_ascii_alphanumeric()
                                    || ch.is_ascii_whitespace()
                                    || ('\u{4e00}'..='\u{9fa5}').contains(&ch)
                                {
                                    ch
                                } else {
                                    '_'
                                }
                            })
                            .collect::<String>();
                        // cap the length: a long caption would produce a file
                        // name too long for the filesystem
                        let cleaned: String = cleaned.trim().chars().take(80).collect();
                        if cleaned.is_empty() {
                            format!("{}.jpg", file_id)
                        } else {
                            format!("{}.jpg", cleaned)
                        }
                    }
                    _ => format!("{}.jpg", file_id),
                };
                if photo.media_group_id.is_some() {
                    // break up the group
                    let old = msg_id;
                    msg_id = (|| bot.send_photo(chat_id, InputFile::file_id(&file_id)))
                        .try_multiple_times(3)
                        .await?
                        .id;
                    if let Err(e) = (|| bot.delete_message(chat_id, old))
                        .try_multiple_times(3)
                        .await
                    {
                        warn!(">> BOT: failed to delete original media-group message {}: {}", old, e);
                    }
                }
                Some((file_id, file_name))
            }
            _ => None,
        } {
            if let Some((old_chat_id, old_msg_id)) = storage
                .set_file_handle(chat_id, msg_id, file_id.clone())
                .await?
            {
                debug_assert_eq!(old_chat_id, chat_id, "chat_id mismatch");
                if (|| bot.delete_message(chat_id, old_msg_id))
                    .try_multiple_times(3)
                    .await
                    .is_ok()
                {
                    info!(">> BOT: deleted message: {}", old_msg_id);
                }
            }
            if chat_state == ChatState::PartiallyActive {
                return Ok(());
            }
            let file_task = tokio::spawn(async move {
                (|| set_emoji(&bot, chat_id, msg_id, "🫡"))
                    .try_multiple_times(3)
                    .await?;
                let emoji = match storage.add_task(file_id, file_name).await? {
                    Some(handle) => match handle.result().await {
                        TransportState::Completed => "👌",
                        TransportState::Cancelled => "😨",
                        TransportState::Failed => "😭",
                        _ => "👾",
                    },
                    None => "👌",
                };
                (|| set_emoji(&bot, chat_id, msg_id, emoji))
                    .try_multiple_times(3)
                    .await?;
                storage
                    .set_file_state_by_handle_and_link((chat_id, msg_id), FileState::Normal)
                    .await
                    .ok();
                Result::<_, anyhow::Error>::Ok(())
            });
            // log background-task errors instead of silently dropping them
            tokio::spawn(async move {
                match file_task.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => warn!(">> BOT: background file processing failed: {}", e),
                    Err(e) => warn!(">> BOT: background file processing panicked: {}", e),
                }
            });
        }
    Ok(())
}

/// sqmusic 下载流程：提交下载 -> 等待任务完成 -> 在（临时）目录找文件 -> 返回歌曲信息卡。
/// 不自动发送音频文件（管理音乐为主）：面板上点 ▶️ 试听才把文件发回。
/// 首选源失败时自动用歌名在 qq/mg 源重试一次；全部失败时通知用户。
/// 配置了 MUSIC_TMP_DIR 时走"临时区试听"流程（下载不直接进音乐库）；
/// 未配置时保持旧行为（下载进音乐库 + 自动刷新 Emby）。
pub(crate) async fn download_and_send(
    bot: Bot,
    sqm: Arc<SqmusicClient>,
    emby: Option<Arc<EmbyClient>>,
    ctx: Context,
    chat_id: ChatId,
    song: MusicRecord,
    br: String,
) -> Result<()> {
    let Some(music_dir) = ctx.music_dir.clone() else {
        bot.send_message(chat_id, "❌ 未配置音乐目录（MUSIC_DIR）").await?;
        return Ok(());
    };
    let mut plugs = vec![song.plugName.clone()];
    for p in ["qq", "mg"] {
        if !plugs.iter().any(|x| x == p) {
            plugs.push(p.to_string());
        }
    }
    let mut last_err: Option<anyhow::Error> = None;
    for (idx, plug) in plugs.iter().enumerate() {
        let target = if idx == 0 {
            song.clone()
        } else {
            // 换源：用歌名重新搜索，取第一条
            match sqm.search(plug, &song.name, 3).await {
                Ok(s) if !s.is_empty() => s[0].clone(),
                Ok(_) => continue,
                Err(e) => {
                    warn!(">> SQMUSIC: fallback search {} failed: {}", plug, e);
                    last_err = Some(e);
                    continue;
                }
            }
        };
        let br = if idx == 0 {
            br.clone()
        } else {
            crate::sqm::pick_br_type(&target.brTypes)
                .unwrap_or_else(|| target.brTypes.first().cloned().unwrap_or_default())
        };
        if idx > 0 {
            info!(">> SQMUSIC: retry via {} source", plug);
        }
        match try_download_and_send(&bot, &sqm, emby.clone(), &ctx, &music_dir, chat_id, &target, &br).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                warn!(">> SQMUSIC: {} attempt failed: {}", plug, e);
                last_err = Some(e);
            }
        }
    }
    let reason = last_err.map(|e| e.to_string()).unwrap_or_else(|| "未知原因".to_string());
    bot.send_message(chat_id, format!("❌ 下载失败：{}", reason)).await?;
    Ok(())
}

/// 单源下载尝试：Emby 预查 -> 本地预查（已有则返回信息卡，不重复下载）-> 提交下载
/// -> 找文件 -> 返回歌曲信息卡。
/// 有临时区（MUSIC_TMP_DIR）时：下载落临时区，返回信息卡 + 面板（▶️ 试听/入库/收藏/删除）；
/// 无临时区时：保持旧行为（下载进音乐库 + 自动刷新 Emby + 信息卡）。
async fn try_download_and_send(
    bot: &Bot,
    sqm: &Arc<SqmusicClient>,
    emby: Option<Arc<EmbyClient>>,
    ctx: &Context,
    music_dir: &PathBuf,
    chat_id: ChatId,
    song: &MusicRecord,
    br: &str,
) -> Result<()> {
    let prefer_ext = crate::sqm::ext_from_br(br);
    // Emby 预查（优先）：Emby 是已索引的音乐库，命中则返回歌曲信息卡（不自动发文件，▶️ 试听才发）
    if let Some(emby) = &emby {
        let artist = if song.artistName.is_empty() {
            None
        } else {
            Some(song.artistName.join(" "))
        };
        match emby.find_song(&song.name, artist.as_deref()).await {
            Ok(Some(found)) => {
                let path = PathBuf::from(&found.Path);
                if path.exists() {
                    info!(">> EMBY: library hit {} -> {}", song.name, found.Path);
                    let act = make_act(music_dir, &path, song, found.Album.clone(), Some(found.Id.clone()), false, br, true);
                    ctx.music_act.lock().insert(chat_id, act.clone());
                    let (text, kb) = trial_panel(&act);
                    send_info_card(bot, chat_id, Some(&emby.cover_url(&found.Id)), Some(&path), text, kb).await;
                    return Ok(());
                }
                // Emby 命中但路径在 bot 容器不可见：降级走本地预查/正常下载
                info!(">> EMBY: hit but path not visible to bot: {}, fallback", found.Path);
            }
            Ok(None) => {}
            Err(e) => {
                warn!(">> EMBY: pre-check failed: {}, fallback to local", e);
            }
        }
    }
    // 本地预查：音乐库已有该歌（sqmusic 判重会跳过下载），返回信息卡（不自动发文件）
    if let Some(found) = crate::sqm::find_in_library(music_dir, song, prefer_ext) {
        info!(">> SQMUSIC: local library hit {}", found.path.display());
        let act = make_act(music_dir, &found.path, song, None, None, false, br, found.format_ok);
        ctx.music_act.lock().insert(chat_id, act.clone());
        let (text, kb) = trial_panel(&act);
        send_info_card(bot, chat_id, None, Some(&found.path), text, kb).await;
        return Ok(());
    }
    // 临时区预查：这首歌正在试听区（上次下载未入库），不重复下载，再发一次信息卡 + 面板
    if let Some(tmp) = &ctx.music_tmp_dir {
        if let Some(found) = crate::sqm::find_in_library(tmp, song, prefer_ext) {
            info!(">> SQMUSIC: tmp hit {}", found.path.display());
            let act = make_act(tmp, &found.path, song, None, None, true, br, found.format_ok);
            ctx.music_act.lock().insert(chat_id, act.clone());
            let (text, kb) = trial_panel(&act);
            send_info_card(bot, chat_id, song.cover_url().as_deref(), Some(&found.path), text, kb).await;
            return Ok(());
        }
    }
    // 未命中：正常下载
    sqm.download_song(song, br).await?;
    let task = sqm.wait_task(&song.id, 90).await?;
    // 有临时区：在临时区找文件 -> 返回信息卡（不自动发文件，▶️ 试听才发）
    if let Some(tmp) = &ctx.music_tmp_dir {
        let found = crate::sqm::find_latest_audio(tmp, &task, prefer_ext).await?;
        let file = found.path;
        let act = make_act(tmp, &file, song, task.downloadAlbumname.clone(), None, true, br, found.format_ok);
        ctx.music_act.lock().insert(chat_id, act.clone());
        info!(">> SQMUSIC: trial downloaded {} (tmp)", file.display());
        let (text, kb) = trial_panel(&act);
        send_info_card(bot, chat_id, song.cover_url().as_deref(), Some(&file), text, kb).await;
        return Ok(());
    }
    // 无临时区（旧行为）：下载进音乐库 + 刷新 Emby + 信息卡（▶️ 试听发库文件）
    let found = crate::sqm::find_latest_audio(music_dir, &task, prefer_ext).await?;
    let file = found.path;
    // 新歌已写入音乐库，触发 Emby 扫描让新歌立即可见（失败不阻断）
    if let Some(emby) = emby.as_ref() {
        if let Err(e) = emby.refresh_library().await {
            warn!(">> EMBY: refresh after download failed: {}", e);
        } else {
            info!(">> EMBY: library refresh triggered after download");
        }
    }
    let act = make_act(music_dir, &file, song, task.downloadAlbumname.clone(), None, false, br, found.format_ok);
    ctx.music_act.lock().insert(chat_id, act.clone());
    let (_, kb) = trial_panel(&act);
    let mut text = format!(
        "🎵 {} - {}\n💽 {}〔{}〕\n✅ 已下载并同步到 Emby 音乐库（点 ▶️ 试听）",
        act.name,
        act.artist,
        act.album.clone().unwrap_or_else(|| "未知专辑".to_string()),
        br.replace('_', " ")
    );
    if !found.format_ok {
        text.push_str("\n⚠️ 音乐库已存在其它格式（sqmusic 判定重复已跳过下载）");
    }
    send_info_card(bot, chat_id, song.cover_url().as_deref(), Some(&file), text, kb).await;
    Ok(())
}

/// 由文件构造试听操作状态（DownloadAct）。
/// `is_tmp`：true=临时区文件（可安全删除）；false=音乐库文件（删除需两步确认）。
/// `emby_id`：音乐库命中且有 Emby 记录时传入，用于删除已入库歌曲。
fn make_act(
    base_dir: &PathBuf,
    file: &PathBuf,
    song: &MusicRecord,
    album: Option<String>,
    emby_id: Option<String>,
    is_tmp: bool,
    br: &str,
    format_ok: bool,
) -> DownloadAct {
    let artist = if song.artistName.is_empty() {
        "未知歌手".to_string()
    } else {
        song.artistName.join("/")
    };
    let rel_path = file
        .strip_prefix(base_dir)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| {
            PathBuf::from(file.file_name().unwrap_or_default().to_string_lossy().to_string())
        });
    DownloadAct {
        tmp_path: file.clone(),
        rel_path,
        name: song.name.clone(),
        artist,
        album: album.or_else(|| song.albumName.clone()),
        created: Instant::now(),
        kept: false,
        is_tmp,
        emby_id,
        br: br.to_string(),
        format_ok,
    }
}

/// 歌曲信息卡 + 操作面板（文案 + 键盘）。
/// 临时区文件：[▶️ 试听] + [📥 入库][❤️ 收藏][🗑 删除]；
/// 音乐库文件：[▶️ 试听]，文案提示已在库/未重复下载。
pub(crate) fn trial_panel(act: &DownloadAct) -> (String, InlineKeyboardMarkup) {
    let mut text = if act.is_tmp {
        format!(
            "🎵 {} - {}\n💽 {}〔{}〕\n（已下载到临时区，未入库；想听点 ▶️，满意可入库，不满意可删除）",
            act.name,
            act.artist,
            act.album.clone().unwrap_or_else(|| "未知专辑".to_string()),
            act.br.replace('_', " ")
        )
    } else {
        format!(
            "🎵 {} - {}\n💽 {}〔{}〕\n（音乐库已有该歌，未重复下载；点 ▶️ 试听，🗑 删除需两步确认）",
            act.name,
            act.artist,
            act.album.clone().unwrap_or_else(|| "未知专辑".to_string()),
            act.br.replace('_', " ")
        )
    };
    if !act.format_ok {
        let want = crate::sqm::ext_from_br(&act.br).unwrap_or("该格式");
        text.push_str(&format!("\n⚠️ 未找到 {} 格式，已返回其它格式", want));
    }
    let kb = if act.is_tmp {
        InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("▶️ 试听", "music:act:play")],
            vec![
                InlineKeyboardButton::callback("📥 入库", "music:act:keep"),
                InlineKeyboardButton::callback("❤️ 收藏", "music:act:fav"),
                InlineKeyboardButton::callback("🗑 删除", "music:act:del"),
            ],
            vec![InlineKeyboardButton::callback("🔙 返回", "music:act:back")],
        ])
    } else {
        // 音乐库文件：试听 + 收藏/删除/加歌单（删除两步确认；无 Emby 关联时删除会提示走 /emby）
        InlineKeyboardMarkup::new(vec![
            vec![InlineKeyboardButton::callback("▶️ 试听", "music:act:play")],
            vec![
                InlineKeyboardButton::callback("❤️ 收藏", "music:act:fav"),
                InlineKeyboardButton::callback("🗑 删除", "music:act:del"),
                InlineKeyboardButton::callback("➕ 歌单", "music:act:playlist"),
            ],
            vec![InlineKeyboardButton::callback("🔙 返回", "music:act:back")],
        ])
    };
    (text, kb)
}

/// 由 Emby 库内歌曲构造管理态操作状态（DownloadAct）。
/// is_tmp=false + emby_id：信息卡可 试听/收藏/删除/加歌单，删除走两步确认。
pub(crate) fn make_act_from_emby(music_dir: &PathBuf, song: &EmbySong) -> DownloadAct {
    let path = PathBuf::from(&song.Path);
    let artist = if song.Artists.is_empty() {
        "未知歌手".to_string()
    } else {
        song.Artists.join("/")
    };
    let br = ext_to_br(&path);
    let rel_path = path
        .strip_prefix(music_dir)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| PathBuf::from(&song.Name));
    DownloadAct {
        tmp_path: path.clone(),
        rel_path,
        name: song.Name.clone(),
        artist,
        album: song.Album.clone(),
        created: Instant::now(),
        kept: true,
        is_tmp: false,
        emby_id: Some(song.Id.clone()),
        br,
        format_ok: true,
    }
}

/// 从文件扩展名推断音质/格式标识（信息卡文案展示用）。
pub(crate) fn ext_to_br(path: &PathBuf) -> String {
    match path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .as_deref()
    {
        Some("flac") => "flac".to_string(),
        Some("ape") => "ape".to_string(),
        Some("wav") => "wav".to_string(),
        Some("m4a") | Some("aac") => "m4a".to_string(),
        Some("mp3") => "320".to_string(),
        _ => "未知格式".to_string(),
    }
}

/// 发送歌曲信息卡：优先封面 URL（下载合法图片则发图），
/// 失败时兜底读本地文件封面（内嵌图/同目录图片），都失败才降级文本（带键盘）。
pub(crate) async fn send_info_card(
    bot: &Bot,
    chat_id: ChatId,
    cover_url: Option<&str>,
    local_path: Option<&PathBuf>,
    text: String,
    kb: InlineKeyboardMarkup,
) {
    let bytes = if let Some(url) = cover_url {
        match fetch_cover_bytes(url).await {
            Some(b) => Some(b),
            None => local_path.and_then(embedded_cover_bytes),
        }
    } else {
        local_path.and_then(embedded_cover_bytes)
    };
    if let Some(bytes) = bytes {
        let mut req = bot.send_photo(chat_id, InputFile::memory(bytes));
        req.payload_mut().caption = Some(text.clone());
        req.payload_mut().reply_markup =
            Some(teloxide::types::ReplyMarkup::InlineKeyboard(kb.clone()));
        if req.await.is_ok() {
            return;
        }
        // 发图失败（封面损坏/超限等）降级为文本
        warn!(">> SQMUSIC: send cover photo failed, fallback to text");
    }
    let mut req = bot.send_message(chat_id, text);
    req.payload_mut().reply_markup = Some(teloxide::types::ReplyMarkup::InlineKeyboard(kb));
    req.await.ok();
}

/// 从本地音频文件读取封面字节：先试同目录图片文件，再试内嵌封面
/// （flac PICTURE 块 / ID3v2 APIC 帧）。返回经过图片魔数校验的字节。
fn embedded_cover_bytes(path: &PathBuf) -> Option<Vec<u8>> {
    // 1) 同目录图片文件（Emby 音频库不读目录图，但 bot 可以直接读）
    if let Some(dir) = path.parent() {
        for name in [
            "cover.jpg", "folder.jpg", "album.jpg", "poster.jpg", "cover.png", "folder.png",
        ] {
            if let Ok(b) = std::fs::read(dir.join(name)) {
                if is_image_bytes(&b) {
                    return Some(b);
                }
            }
        }
    }
    // 2) 文件内嵌封面
    let data = std::fs::read(path).ok()?;
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    let pic = match ext.as_str() {
        "flac" => flac_cover(&data),
        "mp3" | "ogg" => id3_cover(&data),
        _ => None,
    };
    pic.filter(|b| is_image_bytes(b))
}

/// 解析 flac 元数据块，取 PICTURE（type 6）块的图片数据。
fn flac_cover(data: &[u8]) -> Option<Vec<u8>> {
    if !data.starts_with(b"fLaC") {
        return None;
    }
    let mut off = 4usize;
    while off + 4 <= data.len() {
        let header = data[off];
        let last = header & 0x80 != 0;
        let btype = header & 0x7f;
        // flac 元数据块长度是 24-bit（3 字节）
        let len = ((data[off + 1] as usize) << 16)
            | ((data[off + 2] as usize) << 8)
            | data[off + 3] as usize;
        off += 4;
        if off + len > data.len() {
            return None;
        }
        if btype == 6 {
            return flac_picture(&data[off..off + len]);
        }
        off += len;
        if last {
            break;
        }
    }
    None
}

/// 解析 flac PICTURE 块体：type/mime_len/mime/desc_len/desc/宽高深色/数据。
fn flac_picture(b: &[u8]) -> Option<Vec<u8>> {
    if b.len() < 32 {
        return None;
    }
    let mut p = 0usize;
    p += 4; // picture type
    let mime_len = u32::from_be_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]]) as usize;
    p += 4;
    p += mime_len; // mime
    let desc_len = u32::from_be_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]]) as usize;
    p += 4;
    p += desc_len; // desc
    p += 16; // width/height/depth/colors
    if p + 4 > b.len() {
        return None;
    }
    let data_len = u32::from_be_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]]) as usize;
    p += 4;
    if p + data_len > b.len() {
        return None;
    }
    Some(b[p..p + data_len].to_vec())
}

/// 解析 ID3v2 标签，取 APIC 帧的图片数据（v2.3/v2.4 尺寸均可）。
fn id3_cover(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 10 || !data.starts_with(b"ID3") {
        return None;
    }
    let ver = data[3];
    let size = syncsafe(&data[6..10]);
    let mut off = 10usize;
    let end = (10 + size).min(data.len());
    while off + 10 <= end {
        let frame = &data[off..off + 4];
        off += 4;
        let fsize = if ver >= 4 {
            syncsafe(&data[off..off + 4])
        } else {
            u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize
        };
        off += 4;
        let fmt = u16::from_be_bytes([data[off], data[off + 1]]);
        off += 2;
        if frame == b"APIC" {
            // v2.4 帧扩展（data length indicator）
            let body_start = if ver >= 4 && (fmt & 0x40) != 0 {
                off + 4
            } else {
                off
            };
            let body = &data[body_start..(body_start + fsize).min(end)];
            return id3_apic(body);
        }
        off += fsize;
    }
    None
}

/// 解析 ID3v2 APIC 帧体：编码/mime/图片类型/描述后即图片数据。
fn id3_apic(body: &[u8]) -> Option<Vec<u8>> {
    if body.len() < 6 {
        return None;
    }
    let enc = body[0];
    let mut p = 1usize;
    let mime_end = body[p..].iter().position(|&c| c == 0)?;
    p += mime_end + 1; // mime
    if p >= body.len() {
        return None;
    }
    p += 1; // picture type
    let rest = &body[p..];
    let desc_len = if enc == 1 || enc == 2 {
        // utf16 描述以双字节 \0 结束
        rest.windows(2).position(|w| w == [0, 0])? + 2
    } else {
        rest.iter().position(|&c| c == 0)? + 1
    };
    p += desc_len;
    if p >= body.len() {
        return None;
    }
    Some(body[p..].to_vec())
}

/// ID3v2 syncsafe 整数（28bit）。
fn syncsafe(b: &[u8]) -> usize {
    ((b[0] as usize & 0x7f) << 21)
        | ((b[1] as usize & 0x7f) << 14)
        | ((b[2] as usize & 0x7f) << 7)
        | (b[3] as usize & 0x7f)
}

/// 下载封面图片字节；非图片、超限或失败返回 None。
async fn fetch_cover_bytes(url: &str) -> Option<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    if bytes.len() < 100 || bytes.len() > 8_000_000 {
        return None;
    }
    if !is_image_bytes(&bytes) {
        return None;
    }
    Some(bytes.to_vec())
}

fn is_image_bytes(b: &[u8]) -> bool {
    b.len() >= 4
        && ((b[0] == 0xff && b[1] == 0xd8 && b[2] == 0xff) // jpg
            || (b[0] == 0x89 && b[1] == 0x50 && b[2] == 0x4e && b[3] == 0x47) // png
            || (b[0] == b'R' && b[1] == b'I' && b[2] == b'F' && b[3] == b'F') // webp
            || (b[0] == 0x47 && b[1] == 0x49 && b[2] == 0x46)) // gif
}
