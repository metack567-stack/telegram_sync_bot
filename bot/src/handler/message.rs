use super::{
    MyDialogue,
    command::cmd_handler,
    utils::{TryMultipleTimes, set_emoji},
};
use crate::{
    context::Context,
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
    if let Some(sqm) = &ctx.sqmusic
        && let Some(music_dir) = &ctx.music_dir
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
                    ctx.music_pending.lock().remove(&chat_id);
                    let song = pending.songs[n - 1].clone();
                    let br = crate::sqm::pick_br_type(&song.brTypes)
                        .unwrap_or_else(|| song.brTypes.first().cloned().unwrap_or_default());
                    let artist = if song.artistName.is_empty() {
                        "未知歌手".to_string()
                    } else {
                        song.artistName.join("/")
                    };
                    bot.send_message(
                        chat_id,
                        format!("⬇️ 开始下载：{} - {}〔{}〕", song.name, artist, br.replace('_', " ")),
                    )
                    .await?;
                    let sqm = sqm.clone();
                    let music_dir = music_dir.clone();
                    let bot = bot.clone();
                    tokio::spawn(async move {
                        if let Err(e) = download_and_send(bot, sqm, music_dir, chat_id, song, br).await {
                            warn!(">> SQMUSIC: download flow failed: {}", e);
                        }
                    });
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
async fn download_and_send(
    bot: Bot,
    sqm: Arc<SqmusicClient>,
    music_dir: PathBuf,
    chat_id: ChatId,
    song: MusicRecord,
    br: String,
) -> Result<()> {
    sqm.download_song(&song, &br).await?;
    let task = sqm.wait_task(&song.id, 90).await?;
    let file = crate::sqm::find_latest_audio(&music_dir, &task).await?;
    let title = task.downloadMusicname.clone().unwrap_or_else(|| song.name.clone());
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
    info!(">> SQMUSIC: send audio {} to {}", file.display(), chat_id);
    let mut req = bot.send_audio(chat_id, InputFile::file(&file));
    req.payload_mut().title = Some(title);
    req.payload_mut().performer = Some(performer);
    req.await?;
    bot.send_message(chat_id, "✅ 下载完成，已同步到音乐库").await?;
    Ok(())
}
