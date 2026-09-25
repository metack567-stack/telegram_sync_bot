use crate::{
    context::Context,
    storage::MyStorage,
};
use anyhow::Result;
use std::path::PathBuf;
use teloxide::{
    Bot,
    dispatching::{UpdateFilterExt as _, UpdateHandler},
    prelude::Requester as _,
    requests::HasPayload as _,
    types::{CallbackQuery, ChatId, InputFile, Update},
};
use tracing::{info, warn};

pub fn callback_handler() -> UpdateHandler<anyhow::Error> {
    Update::filter_callback_query().endpoint(handle)
}

async fn handle(
    bot: Bot,
    q: CallbackQuery,
    ctx: Context,
    storage: MyStorage,
) -> Result<()> {
    // always answer the callback so the client stops its loading spinner
    let _ = bot.answer_callback_query(q.id.clone()).await;

    let data = q.data.clone().unwrap_or_default();
    if !data.starts_with("clear:")
        && !data.starts_with("music:dl:")
        && !data.starts_with("music:pick:")
        && !data.starts_with("music:add:")
        && !data.starts_with("emby:pick:")
        && !data.starts_with("emby:add:")
    {
        return Ok(());
    }
    // owner-only: anyone who can see the message could tap the button otherwise
    if !ctx
        .bypass_users
        .as_ref()
        .is_some_and(|users| users.contains(&q.from.id))
    {
        info!(">> BOT: callback from unauthorized user {}", q.from.id);
        return Ok(());
    }
    let Some(chat_id) = q.message.as_ref().map(|m| m.chat().id) else {
        return Ok(());
    };
    let Some(msg_id) = q.message.as_ref().map(|m| m.id()) else {
        return Ok(());
    };
    match data.split(':').next().unwrap_or_default() {
        "clear" => handle_clear(bot, chat_id, msg_id, data, storage).await,
        "music" => handle_music(bot, ctx, chat_id, msg_id, data).await,
        "emby" => handle_emby(bot, ctx, chat_id, msg_id, data).await,
        _ => Ok(()),
    }
}

async fn handle_clear(
    bot: Bot,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    data: String,
    storage: MyStorage,
) -> Result<()> {
    match data.as_str() {
        "clear:yes" => match storage.clear_normal().await {
            Ok((count, freed)) => {
                let text = format!(
                    "已清空 {} 个文件，释放 {}",
                    count,
                    crate::utils::format_size(freed)
                );
                let mut req = bot.edit_message_text(chat_id, msg_id, text);
                req.payload_mut().reply_markup = None;
                req.await.ok();
                info!(">> BOT: cleared {} normal file(s)", count);
            }
            Err(e) => {
                let mut req =
                    bot.edit_message_text(chat_id, msg_id, format!("清空失败：{}", e));
                req.payload_mut().reply_markup = None;
                req.await.ok();
                warn!(">> BOT: clear failed: {}", e);
            }
        },
        "clear:no" => {
            let mut req = bot.edit_message_text(chat_id, msg_id, "已取消，未做任何更改");
            req.payload_mut().reply_markup = None;
            req.await.ok();
            info!(">> BOT: clear cancelled");
        }
        _ => {}
    }
    Ok(())
}

/// 歌曲/音质回调入口：music:pick:<idx>（选歌）与 music:dl:<idx>:<br>（选音质并下载）。
async fn handle_music(
    bot: Bot,
    ctx: Context,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    data: String,
) -> Result<()> {
    let parts: Vec<&str> = data.split(':').collect();
    match parts.get(1) {
        Some(&"pick") => handle_music_pick(bot, ctx, chat_id, msg_id, &parts).await,
        Some(&"dl") => handle_music_dl(bot, ctx, chat_id, msg_id, &parts).await,
        Some(&"add") => handle_music_add(bot, ctx, chat_id, msg_id, &parts).await,
        _ => Ok(()),
    }
}

/// 点击候选歌曲按钮：选中该歌并把当前消息原地换成音质选择键盘。
async fn handle_music_pick(
    bot: Bot,
    ctx: Context,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    parts: &[&str],
) -> Result<()> {
    let Some(idx) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) else {
        return Ok(());
    };
    let pending = ctx.music_pending.lock().get(&chat_id).cloned();
    let Some(pending) = pending else {
        let mut req = bot.edit_message_text(chat_id, msg_id, "❌ 选择已过期，请重新 /music 搜索");
        req.payload_mut().reply_markup = None;
        req.await.ok();
        return Ok(());
    };
    if pending.expired() {
        ctx.music_pending.lock().remove(&chat_id);
        let mut req = bot.edit_message_text(chat_id, msg_id, "❌ 选择已过期，请重新 /music 搜索");
        req.payload_mut().reply_markup = None;
        req.await.ok();
        return Ok(());
    }
    let Some(song) = pending.songs.get(idx).cloned() else {
        return Ok(());
    };
    let mut p = pending.clone();
    p.chosen = Some(idx);
    ctx.music_pending.lock().insert(chat_id, p);
    let kb = crate::sqm::quality_keyboard(idx, &song.brTypes);
    let artist = if song.artistName.is_empty() {
        "未知歌手".to_string()
    } else {
        song.artistName.join("/")
    };
    let mut req = bot.edit_message_text(
        chat_id,
        msg_id,
        format!("🎚️ 请选择「{} - {}」的音质：", song.name, artist),
    );
    req.payload_mut().reply_markup = Some(kb);
    req.await.ok();
    Ok(())
}

/// 点击音质按钮：校验选择后提交下载。
async fn handle_music_dl(
    bot: Bot,
    ctx: Context,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    parts: &[&str],
) -> Result<()> {
    // parts == ["music", "dl", "<idx>", "<br>"]
    let (Some(idx), Some(br)) = (
        parts.get(2).and_then(|s| s.parse::<usize>().ok()),
        parts.get(3).map(|s| s.to_string()),
    ) else {
        return Ok(());
    };
    let pending = ctx.music_pending.lock().get(&chat_id).cloned();
    let Some(pending) = pending else {
        let mut req = bot.edit_message_text(chat_id, msg_id, "❌ 选择已过期，请重新 /music 搜索");
        req.payload_mut().reply_markup = None;
        req.await.ok();
        return Ok(());
    };
    if pending.expired() {
        ctx.music_pending.lock().remove(&chat_id);
        let mut req = bot.edit_message_text(chat_id, msg_id, "❌ 选择已过期，请重新 /music 搜索");
        req.payload_mut().reply_markup = None;
        req.await.ok();
        return Ok(());
    }
    if pending.chosen != Some(idx) {
        // 按钮来自旧的歌曲列表（数字与当前选择不一致）
        let mut req = bot.edit_message_text(chat_id, msg_id, "❌ 歌曲列表已更新，请重新 /music 搜索");
        req.payload_mut().reply_markup = None;
        req.await.ok();
        return Ok(());
    }
    let Some(song) = pending.songs.get(idx).cloned() else {
        return Ok(());
    };
    ctx.music_pending.lock().remove(&chat_id);
    let br = if br == "auto" {
        crate::sqm::pick_br_type_with_pref(&song.brTypes, pending.pref.as_deref())
            .unwrap_or_else(|| song.brTypes.first().cloned().unwrap_or_default())
    } else {
        br
    };
    let artist = if song.artistName.is_empty() {
        "未知歌手".to_string()
    } else {
        song.artistName.join("/")
    };
    let mut req = bot.edit_message_text(
        chat_id,
        msg_id,
        format!("⬇️ 开始下载：{} - {}〔{}〕", song.name, artist, br.replace('_', " ")),
    );
    req.payload_mut().reply_markup = None;
    req.await.ok();
    if let (Some(sqm), Some(music_dir)) = (&ctx.sqmusic, &ctx.music_dir) {
        let sqm = sqm.clone();
        let music_dir = music_dir.clone();
        let emby = ctx.emby.clone();
        let bot = bot.clone();
        tokio::spawn(async move {
            if let Err(e) = super::message::download_and_send(bot, sqm, emby, music_dir, chat_id, song, br).await {
                warn!(">> SQMUSIC: download flow failed: {}", e);
            }
        });
    }
    Ok(())
}

/// Emby 点播回调入口：emby:pick:<idx>。
async fn handle_emby(
    bot: Bot,
    ctx: Context,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    data: String,
) -> Result<()> {
    let parts: Vec<&str> = data.split(':').collect();
    match parts.get(1) {
        Some(&"pick") => handle_emby_pick(bot, ctx, chat_id, msg_id, &parts).await,
        Some(&"add") => handle_emby_add(bot, ctx, chat_id, msg_id, &parts).await,
        _ => Ok(()),
    }
}

/// 点击 Emby 候选歌曲按钮：校验选择后把库里的音频文件发回。
async fn handle_emby_pick(
    bot: Bot,
    ctx: Context,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    parts: &[&str],
) -> Result<()> {
    let Some(idx) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) else {
        return Ok(());
    };
    let pending = ctx.emby_pending.lock().get(&chat_id).cloned();
    let Some(pending) = pending else {
        let mut req = bot.edit_message_text(chat_id, msg_id, "❌ 选择已过期，请重新 /emby 搜索");
        req.payload_mut().reply_markup = None;
        req.await.ok();
        return Ok(());
    };
    if pending.expired() {
        ctx.emby_pending.lock().remove(&chat_id);
        let mut req = bot.edit_message_text(chat_id, msg_id, "❌ 选择已过期，请重新 /emby 搜索");
        req.payload_mut().reply_markup = None;
        req.await.ok();
        return Ok(());
    }
    let Some(song) = pending.songs.get(idx).cloned() else {
        return Ok(());
    };
    ctx.emby_pending.lock().remove(&chat_id);
    let path = PathBuf::from(&song.Path);
    if !path.exists() {
        let mut req = bot.edit_message_text(
            chat_id,
            msg_id,
            format!("⚠️ 文件不可见：{}（挂载不一致？）", song.Path),
        );
        req.payload_mut().reply_markup = None;
        req.await.ok();
        return Ok(());
    }
    let artist = if song.Artists.is_empty() {
        "未知歌手".to_string()
    } else {
        song.Artists.join("/")
    };
    let mut req = bot.edit_message_text(
        chat_id,
        msg_id,
        format!("📤 正在发送：{} - {}", song.Name, artist),
    );
    req.payload_mut().reply_markup = None;
    req.await.ok();
    info!(">> EMBY: play {} -> {}", song.Name, song.Path);
    let mut req = bot.send_audio(chat_id, InputFile::file(&path));
    req.payload_mut().title = Some(song.Name.clone());
    req.payload_mut().performer = Some(artist);
    req.await?;
    Ok(())
}

/// 点击 ➕ 按钮：把选中的 Emby 歌曲加入当前歌单（/playlist 创建/打开）。
async fn handle_emby_add(
    bot: Bot,
    ctx: Context,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    parts: &[&str],
) -> Result<()> {
    let Some(idx) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) else {
        return Ok(());
    };
    let pending = ctx.emby_pending.lock().get(&chat_id).cloned();
    let Some(pending) = pending else {
        let mut req = bot.edit_message_text(chat_id, msg_id, "❌ 选择已过期，请重新 /emby 搜索");
        req.payload_mut().reply_markup = None;
        req.await.ok();
        return Ok(());
    };
    if pending.expired() {
        ctx.emby_pending.lock().remove(&chat_id);
        let mut req = bot.edit_message_text(chat_id, msg_id, "❌ 选择已过期，请重新 /emby 搜索");
        req.payload_mut().reply_markup = None;
        req.await.ok();
        return Ok(());
    }
    let Some(song) = pending.songs.get(idx).cloned() else {
        return Ok(());
    };
    let Some(playlist) = ctx.playlist.lock().clone() else {
        let mut req = bot.edit_message_text(chat_id, msg_id, "❌ 未设置歌单，先用 /playlist <歌单名> 创建");
        req.payload_mut().reply_markup = None;
        req.await.ok();
        return Ok(());
    };
    let Some(emby) = ctx.emby.clone() else {
        return Ok(());
    };
    let mut req = bot.edit_message_text(
        chat_id,
        msg_id,
        format!("➕ 正在加入歌单「{}」：{}", playlist.name, song.Name),
    );
    req.payload_mut().reply_markup = None;
    req.await.ok();
    match emby.add_to_playlist(&playlist.id, &song.Id).await {
        Ok(()) => {
            info!(">> EMBY: add {} to playlist {} ({})", song.Name, playlist.name, playlist.id);
            bot.send_message(
                chat_id,
                format!("➕ 已加入歌单「{}」：{}", playlist.name, song.Name),
            )
            .await?;
        }
        Err(e) => {
            warn!(">> EMBY: add to playlist failed: {}", e);
            bot.send_message(chat_id, format!("❌ 加入歌单失败：{}", e)).await?;
        }
    }
    Ok(())
}

/// 点击 /music 候选列表的 ➕ 按钮：把选中的 sqmusic 歌曲加入当前 Emby 歌单。
/// 歌曲需已在 Emby 音乐库（已下载并被扫描）；未入库时提示先下载。
async fn handle_music_add(
    bot: Bot,
    ctx: Context,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    parts: &[&str],
) -> Result<()> {
    let Some(idx) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) else {
        return Ok(());
    };
    let pending = ctx.music_pending.lock().get(&chat_id).cloned();
    let Some(pending) = pending else {
        let mut req = bot.edit_message_text(chat_id, msg_id, "❌ 选择已过期，请重新 /music 搜索");
        req.payload_mut().reply_markup = None;
        req.await.ok();
        return Ok(());
    };
    if pending.expired() {
        ctx.music_pending.lock().remove(&chat_id);
        let mut req = bot.edit_message_text(chat_id, msg_id, "❌ 选择已过期，请重新 /music 搜索");
        req.payload_mut().reply_markup = None;
        req.await.ok();
        return Ok(());
    }
    let Some(song) = pending.songs.get(idx).cloned() else {
        return Ok(());
    };
    let Some(playlist) = ctx.playlist.lock().clone() else {
        let mut req = bot.edit_message_text(chat_id, msg_id, "❌ 未设置歌单，先用 /playlist <歌单名> 创建");
        req.payload_mut().reply_markup = None;
        req.await.ok();
        return Ok(());
    };
    let Some(emby) = ctx.emby.clone() else {
        return Ok(());
    };
    let mut req = bot.edit_message_text(
        chat_id,
        msg_id,
        format!("🔎 正在查询音乐库：{} ...", song.name),
    );
    req.payload_mut().reply_markup = None;
    req.await.ok();
    let artist = if song.artistName.is_empty() {
        String::new()
    } else {
        song.artistName.join(" ")
    };
    match emby.find_song(&song.name, (!artist.is_empty()).then_some(artist.as_str())).await {
        Ok(Some(found)) => match emby.add_to_playlist(&playlist.id, &found.Id).await {
            Ok(()) => {
                info!(">> EMBY: add {} to playlist {} ({})", found.Name, playlist.name, playlist.id);
                bot.send_message(
                    chat_id,
                    format!("➕ 已加入歌单「{}」：{}", playlist.name, found.Name),
                )
                .await?;
            }
            Err(e) => {
                warn!(">> EMBY: add to playlist failed: {}", e);
                bot.send_message(chat_id, format!("❌ 加入歌单失败：{}", e)).await?;
            }
        },
        Ok(None) => {
            bot.send_message(
                chat_id,
                format!(
                    "⚠️ 「{}」还没在 Emby 音乐库（未下载或未扫描完成）。先 /music 下载，等扫描完成后可再加入歌单",
                    song.name
                ),
            )
            .await?;
        }
        Err(e) => {
            bot.send_message(chat_id, format!("❌ 查询音乐库失败：{}", e)).await?;
        }
    }
    Ok(())
}
