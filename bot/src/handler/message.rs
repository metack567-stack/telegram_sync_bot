use super::{
    MyDialogue,
    command::cmd_handler,
    utils::{TryMultipleTimes, set_emoji},
};
use crate::{
    context::Context,
    emby::EmbyClient,
    sqm::{MusicRecord, SqmusicClient},
    storage::{ChatState, FileState, MyStorage, TransportState},
};
use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use teloxide::{
    Bot,
    dispatching::{UpdateFilterExt as _, UpdateHandler},
    prelude::Requester as _,
    requests::HasPayload as _,
    types::{ChatId, InputFile, MediaKind, MediaText, Message, MessageKind, Update},
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

/// sqmusic 下载流程：提交下载 -> 等待任务完成 -> 在音乐目录找文件 -> 发回 Telegram。
/// 首选源失败时自动用歌名在 qq/mg 源重试一次；全部失败时通知用户。
pub(crate) async fn download_and_send(
    bot: Bot,
    sqm: Arc<SqmusicClient>,
    emby: Option<Arc<EmbyClient>>,
    music_dir: PathBuf,
    chat_id: ChatId,
    song: MusicRecord,
    br: String,
) -> Result<()> {
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
        match try_download_and_send(&bot, &sqm, emby.clone(), &music_dir, chat_id, &target, &br).await {
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

/// 发回音频文件（带标题/歌手），返回文件名。失败冒泡给调用方处理。
async fn send_audio_with_title(
    bot: &Bot,
    chat_id: ChatId,
    file: &PathBuf,
    title: String,
    performer: String,
) -> Result<String> {
    info!(">> SQMUSIC: send audio {} to {}", file.display(), chat_id);
    let mut req = bot.send_audio(chat_id, InputFile::file(file));
    req.payload_mut().title = Some(title);
    req.payload_mut().performer = Some(performer);
    req.await?;
    Ok(file
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default())
}

/// 单源下载尝试：Emby 预查 -> 本地预查（已有则直接返回，不重复下载）-> 提交下载
/// -> 找文件 -> 发回音频 + 完成消息。
async fn try_download_and_send(
    bot: &Bot,
    sqm: &Arc<SqmusicClient>,
    emby: Option<Arc<EmbyClient>>,
    music_dir: &PathBuf,
    chat_id: ChatId,
    song: &MusicRecord,
    br: &str,
) -> Result<()> {
    let prefer_ext = crate::sqm::ext_from_br(br);
    // Emby 预查（优先）：Emby 是已索引的音乐库，命中则直接发回已有文件，不重复下载
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
                    let title = song.name.clone();
                    let performer = if song.artistName.is_empty() {
                        "未知歌手".to_string()
                    } else {
                        song.artistName.join("/")
                    };
                    let fname = send_audio_with_title(bot, chat_id, &path, title, performer).await?;
                    bot.send_message(
                        chat_id,
                        format!("✅ Emby 音乐库已有该歌（{}），未重复下载", fname),
                    )
                    .await?;
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
    // 本地预查：音乐库已有该歌（sqmusic 判重会跳过下载），直接发回已有文件
    if let Some(found) = crate::sqm::find_in_library(music_dir, song, prefer_ext) {
        let title = song.name.clone();
        let performer = if song.artistName.is_empty() {
            "未知歌手".to_string()
        } else {
            song.artistName.join("/")
        };
        let fname = send_audio_with_title(bot, chat_id, &found.path, title, performer).await?;
        if found.format_ok {
            bot.send_message(
                chat_id,
                format!(
                    "✅ 音乐库已有该歌（{}〔{}〕），未重复下载",
                    fname,
                    br.replace('_', " ")
                ),
            )
            .await?;
        } else {
            let want = prefer_ext.unwrap_or("该格式");
            bot.send_message(
                chat_id,
                format!(
                    "⚠️ 音乐库已有该歌（{}），但不是 {} 格式；sqmusic 判定重复会跳过下载。如需 {} 请先在音乐库删除旧文件再试",
                    fname, want, want
                ),
            )
            .await?;
        }
        return Ok(());
    }
    // 未命中：正常下载
    sqm.download_song(song, br).await?;
    let task = sqm.wait_task(&song.id, 90).await?;
    let found = crate::sqm::find_latest_audio(music_dir, &task, prefer_ext).await?;
    let file = found.path;
    let title = task
        .downloadMusicname
        .clone()
        .unwrap_or_else(|| song.name.clone());
    let performer = task
        .downloadArtistname
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if song.artistName.is_empty() {
                "未知歌手".to_string()
            } else {
                song.artistName.join("/")
            }
        });
    let fname = send_audio_with_title(bot, chat_id, &file, title, performer).await?;
    if found.format_ok {
        bot.send_message(
            chat_id,
            format!("✅ 下载完成：{}〔{}〕，已同步到音乐库", fname, br.replace('_', " ")),
        )
        .await?;
    } else {
        let want = prefer_ext.unwrap_or("该格式");
        bot.send_message(
            chat_id,
            format!(
                "⚠️ 音乐库已存在该歌（{}），但不是 {} 格式；sqmusic 判定重复已跳过下载，已返回现有文件。如需 {} 请先在音乐库删除旧文件再试",
                fname, want, want
            ),
        )
        .await?;
    }
    Ok(())
}
