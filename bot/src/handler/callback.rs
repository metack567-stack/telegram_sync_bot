use crate::{
    context::Context,
    emby::{PendingEmby, PlaylistCtx},
    storage::MyStorage,
};
use anyhow::Result;
use std::path::PathBuf;
use std::time::Instant;
use teloxide::{
    Bot,
    dispatching::{UpdateFilterExt as _, UpdateHandler},
    prelude::Requester as _,
    requests::HasPayload as _,
    types::{
        CallbackQuery, ChatId, InlineKeyboardButton, InlineKeyboardMarkup, InputFile, Update,
    },
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
        && !data.starts_with("music:act:")
        && !data.starts_with("emby:pick:")
        && !data.starts_with("emby:add:")
        && !data.starts_with("favs:")
        && !data.starts_with("playlist:")
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
        "music" => handle_music(bot, ctx, storage, chat_id, msg_id, data).await,
        "emby" => handle_emby(bot, ctx, chat_id, msg_id, data).await,
        "favs" => handle_favs(bot, ctx, storage, chat_id, msg_id, data).await,
        "playlist" => handle_playlist_cb(bot, ctx, chat_id, msg_id, data).await,
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
                edit_message(&bot, chat_id, msg_id, text).await;
                info!(">> BOT: cleared {} normal file(s)", count);
            }
            Err(e) => {
                edit_message(&bot, chat_id, msg_id, format!("清空失败：{}", e)).await;
                warn!(">> BOT: clear failed: {}", e);
            }
        },
        "clear:no" => {
            edit_message(&bot, chat_id, msg_id, "已取消，未做任何更改").await;
            info!(">> BOT: clear cancelled");
        }
        _ => {}
    }
    Ok(())
}

/// 歌曲/音质/试听操作回调入口：
/// music:pick:<idx>（选歌）、music:dl:<idx>:<br>（选音质并下载）、music:act:<keep|fav|del|playlist>（试听后操作）。
async fn handle_music(
    bot: Bot,
    ctx: Context,
    storage: MyStorage,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    data: String,
) -> Result<()> {
    let parts: Vec<&str> = data.split(':').collect();
    match parts.get(1) {
        Some(&"pick") => handle_music_pick(bot, ctx, chat_id, msg_id, &parts).await,
        Some(&"dl") => handle_music_dl(bot, ctx, chat_id, msg_id, &parts).await,
        Some(&"act") => handle_music_act(bot, ctx, storage, chat_id, msg_id, &parts).await,
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
        edit_message(&bot, chat_id, msg_id, "❌ 选择已过期，请重新 /music 搜索").await;
        return Ok(());
    };
    if pending.expired() {
        ctx.music_pending.lock().remove(&chat_id);
        edit_message(&bot, chat_id, msg_id, "❌ 选择已过期，请重新 /music 搜索").await;
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
    edit_message_with_kb(
        &bot,
        chat_id,
        msg_id,
        format!("🎚️ 请选择「{} - {}」的音质：", song.name, artist),
        kb,
    )
    .await;
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
        edit_message(&bot, chat_id, msg_id, "❌ 选择已过期，请重新 /music 搜索").await;
        return Ok(());
    };
    if pending.expired() {
        ctx.music_pending.lock().remove(&chat_id);
        edit_message(&bot, chat_id, msg_id, "❌ 选择已过期，请重新 /music 搜索").await;
        return Ok(());
    }
    if pending.chosen != Some(idx) {
        // 按钮来自旧的歌曲列表（数字与当前选择不一致）
        edit_message(&bot, chat_id, msg_id, "❌ 歌曲列表已更新，请重新 /music 搜索").await;
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
    edit_message(
        &bot,
        chat_id,
        msg_id,
        format!("⬇️ 开始下载：{} - {}〔{}〕", song.name, artist, br.replace('_', " ")),
    )
    .await;
    if let (Some(sqm), Some(_music_dir)) = (&ctx.sqmusic, &ctx.music_dir) {
        let sqm = sqm.clone();
        let ctx = ctx.clone();
        let emby = ctx.emby.clone();
        let bot = bot.clone();
        tokio::spawn(async move {
            if let Err(e) = super::message::download_and_send(bot, sqm, emby, ctx, chat_id, song, br).await {
                warn!(">> SQMUSIC: download flow failed: {}", e);
            }
        });
    }
    Ok(())
}

/// 试听操作面板：music:act:<keep|fav|del|playlist>。
/// keep = 移入音乐库（copy + 删临时）+ Emby 刷新；fav = 写收藏（文件保留）；
/// del = 删临时文件；playlist = 入库后把歌加入当前 Emby 歌单。
async fn handle_music_act(
    bot: Bot,
    ctx: Context,
    storage: MyStorage,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    parts: &[&str],
) -> Result<()> {
    let act = parts.get(2).unwrap_or(&"").to_string();
    let pending = ctx.music_act.lock().get(&chat_id).cloned();
    let Some(mut pending) = pending else {
        edit_message(&bot, chat_id, msg_id, "❌ 操作已过期，请重新 /music 下载试听").await;
        return Ok(());
    };
    if pending.expired() {
        ctx.music_act.lock().remove(&chat_id);
        edit_message(&bot, chat_id, msg_id, "❌ 操作已过期（10 分钟），请重新 /music 下载试听").await;
        return Ok(());
    }
    match act.as_str() {
        "keep" => {
            let Some(music_dir) = ctx.music_dir.clone() else {
                edit_message(&bot, chat_id, msg_id, "❌ 未配置音乐目录（MUSIC_DIR）").await;
                return Ok(());
            };
            let target = music_dir.join(&pending.rel_path);
            if let Some(parent) = target.parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    edit_message(&bot, chat_id, msg_id, format!("❌ 入库失败：{}", e)).await;
                    return Ok(());
                }
            }
            // 临时区与音乐库是两个挂载点，rename 会 EXDEV：用 copy + 删临时
            match tokio::fs::copy(&pending.tmp_path, &target).await {
                Ok(_) => {
                    tokio::fs::remove_file(&pending.tmp_path).await.ok();
                    pending.kept = true;
                    ctx.music_act.lock().insert(chat_id, pending.clone());
                    info!(">> MUSIC: kept {} -> {}", pending.name, target.display());
                    if let Some(emby) = &ctx.emby {
                        if let Err(e) = emby.refresh_library().await {
                            warn!(">> EMBY: refresh after keep failed: {}", e);
                        }
                    }
                    let text = format!(
                        "✅ 已入库：{} - {}\n（音乐库已同步，可加入歌单）",
                        pending.name, pending.artist
                    );
                    let kb = InlineKeyboardMarkup::new(vec![vec![
                        InlineKeyboardButton::callback("➕ 加入歌单", "music:act:playlist"),
                        InlineKeyboardButton::callback("❤️ 收藏", "music:act:fav"),
                    ]]);
                    edit_message_with_kb(&bot, chat_id, msg_id, text, kb).await;
                }
                Err(e) => {
                    edit_message(&bot, chat_id, msg_id, format!("❌ 入库失败：{}", e)).await;
                }
            }
        }
        "fav" => {
            let album = pending.album.clone().unwrap_or_default();
            match storage.add_favorite(chat_id, &pending.name, &pending.artist, &album).await {
                Ok(true) => {
                    info!(">> FAVS: added {} - {}", pending.name, pending.artist);
                    let text = format!(
                        "❤️ 已收藏：{} - {}\n（试听文件保留，可继续 📥入库 或 🗑删除）",
                        pending.name, pending.artist
                    );
                    let kb = InlineKeyboardMarkup::new(vec![vec![
                        InlineKeyboardButton::callback("📥 入库", "music:act:keep"),
                        InlineKeyboardButton::callback("🗑 删除", "music:act:del"),
                    ]]);
                    edit_message_with_kb(&bot, chat_id, msg_id, text, kb).await;
                }
                Ok(false) => {
                    edit_message(
                        &bot,
                        chat_id,
                        msg_id,
                        format!("❤️ 已在收藏中：{} - {}", pending.name, pending.artist),
                    )
                    .await;
                }
                Err(e) => {
                    edit_message(&bot, chat_id, msg_id, format!("❌ 收藏失败：{}", e)).await;
                }
            }
        }
        "del" => {
            match tokio::fs::remove_file(&pending.tmp_path).await {
                Ok(_) => {
                    ctx.music_act.lock().remove(&chat_id);
                    info!(">> MUSIC: deleted tmp {}", pending.tmp_path.display());
                    edit_message(
                        &bot,
                        chat_id,
                        msg_id,
                        format!("🗑 已删除试听文件：{} - {}", pending.name, pending.artist),
                    )
                    .await;
                }
                Err(e) => {
                    edit_message(&bot, chat_id, msg_id, format!("❌ 删除失败：{}", e)).await;
                }
            }
        }
        "playlist" => {
            if !pending.kept {
                edit_message(&bot, chat_id, msg_id, "⚠️ 请先 📥 入库，才能加入歌单").await;
                return Ok(());
            }
            let Some(playlist) = ctx.playlist.lock().clone() else {
                edit_message(&bot, chat_id, msg_id, "❌ 未设置歌单，先用 /playlist <歌单名> 创建").await;
                return Ok(());
            };
            let Some(emby) = ctx.emby.clone() else {
                return Ok(());
            };
            let artist = if pending.artist == "未知歌手" {
                None
            } else {
                Some(pending.artist.as_str())
            };
            match emby.find_song(&pending.name, artist).await {
                Ok(Some(found)) => match emby.add_to_playlist(&playlist.id, &found.Id).await {
                    Ok(()) => {
                        info!(
                            ">> EMBY: add {} to playlist {} ({})",
                            found.Name, playlist.name, playlist.id
                        );
                        bot.send_message(
                            chat_id,
                            format!("➕ 已加入歌单「{}」：{}", playlist.name, found.Name),
                        )
                        .await?;
                        edit_message(
                            &bot,
                            chat_id,
                            msg_id,
                            format!("✅ 已入库并加入歌单「{}」：{}", playlist.name, pending.name),
                        )
                        .await;
                    }
                    Err(e) => {
                        warn!(">> EMBY: add to playlist failed: {}", e);
                        bot.send_message(chat_id, format!("❌ 加入歌单失败：{}", e)).await?;
                    }
                },
                Ok(None) => {
                    bot.send_message(
                        chat_id,
                        format!("⚠️ 音乐库还没扫到「{}」，稍等几分钟再试（或 /emby {} 确认）", pending.name, pending.name),
                    )
                    .await?;
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("❌ 查询音乐库失败：{}", e)).await?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Emby 点播回调入口：emby:pick:<idx> / emby:add:<idx>。
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
        edit_message(&bot, chat_id, msg_id, "❌ 选择已过期，请重新 /emby 搜索").await;
        return Ok(());
    };
    if pending.expired() {
        ctx.emby_pending.lock().remove(&chat_id);
        edit_message(&bot, chat_id, msg_id, "❌ 选择已过期，请重新 /emby 搜索").await;
        return Ok(());
    }
    let Some(song) = pending.songs.get(idx).cloned() else {
        return Ok(());
    };
    ctx.emby_pending.lock().remove(&chat_id);
    let path = PathBuf::from(&song.Path);
    if !path.exists() {
        edit_message(
            &bot,
            chat_id,
            msg_id,
            format!("⚠️ 文件不可见：{}（挂载不一致？）", song.Path),
        )
        .await;
        return Ok(());
    }
    let artist = if song.Artists.is_empty() {
        "未知歌手".to_string()
    } else {
        song.Artists.join("/")
    };
    edit_message(&bot, chat_id, msg_id, format!("📤 正在发送：{} - {}", song.Name, artist)).await;
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
        edit_message(&bot, chat_id, msg_id, "❌ 选择已过期，请重新 /emby 搜索").await;
        return Ok(());
    };
    if pending.expired() {
        ctx.emby_pending.lock().remove(&chat_id);
        edit_message(&bot, chat_id, msg_id, "❌ 选择已过期，请重新 /emby 搜索").await;
        return Ok(());
    }
    let Some(song) = pending.songs.get(idx).cloned() else {
        return Ok(());
    };
    let Some(playlist) = ctx.playlist.lock().clone() else {
        edit_message(&bot, chat_id, msg_id, "❌ 未设置歌单，先用 /playlist <歌单名> 创建").await;
        return Ok(());
    };
    let Some(emby) = ctx.emby.clone() else {
        return Ok(());
    };
    edit_message(
        &bot,
        chat_id,
        msg_id,
        format!("➕ 正在加入歌单「{}」：{}", playlist.name, song.Name),
    )
    .await;
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

/// 收藏列表回调：favs:play:<idx>（播放收藏）/ favs:del:<idx>（取消收藏）。
async fn handle_favs(
    bot: Bot,
    ctx: Context,
    storage: MyStorage,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    data: String,
) -> Result<()> {
    let parts: Vec<&str> = data.split(':').collect();
    let Some(idx) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) else {
        return Ok(());
    };
    let favs = storage.list_favorites(chat_id).await?;
    let Some(fav) = favs.get(idx).cloned() else {
        return Ok(());
    };
    match parts.get(1) {
        Some(&"play") => {
            let Some(emby) = ctx.emby.clone() else {
                edit_message(&bot, chat_id, msg_id, "Emby 联动未启用").await;
                return Ok(());
            };
            let artist = if fav.artist.is_empty() || fav.artist == "未知歌手" {
                None
            } else {
                Some(fav.artist.as_str())
            };
            match emby.find_song(&fav.name, artist).await {
                Ok(Some(found)) => {
                    let path = PathBuf::from(&found.Path);
                    if !path.exists() {
                        edit_message(
                            &bot,
                            chat_id,
                            msg_id,
                            format!("⚠️ 文件不可见：{}（挂载不一致？）", found.Path),
                        )
                        .await;
                        return Ok(());
                    }
                    let artist_show = if found.Artists.is_empty() {
                        "未知歌手".to_string()
                    } else {
                        found.Artists.join("/")
                    };
                    edit_message(
                        &bot,
                        chat_id,
                        msg_id,
                        format!("📤 正在发送收藏：{} - {}", found.Name, artist_show),
                    )
                    .await;
                    info!(">> FAVS: play {} -> {}", found.Name, found.Path);
                    let mut req = bot.send_audio(chat_id, InputFile::file(&path));
                    req.payload_mut().title = Some(found.Name.clone());
                    req.payload_mut().performer = Some(artist_show.clone());
                    req.await?;
                    // 附一条"取消收藏"操作
                    let kb = InlineKeyboardMarkup::new(vec![vec![
                        InlineKeyboardButton::callback("🗑 取消收藏", format!("favs:del:{}", idx)),
                    ]]);
                    let mut req = bot.send_message(
                        chat_id,
                        format!("❤️ 正在播放收藏：{} - {}\n（不喜欢了可取消收藏）", fav.name, artist_show),
                    );
                    req.payload_mut().reply_markup =
                        Some(teloxide::types::ReplyMarkup::InlineKeyboard(kb));
                    req.await?;
                }
                Ok(None) => {
                    bot.send_message(
                        chat_id,
                        format!(
                            "⚠️ 「{}」不在 Emby 音乐库（可能已被删除），用 /music 重新下载",
                            fav.name
                        ),
                    )
                    .await?;
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("❌ 查询失败：{}", e)).await?;
                }
            }
        }
        Some(&"del") => {
            match storage.remove_favorite(chat_id, &fav.name, &fav.artist).await {
                Ok(true) => {
                    info!(">> FAVS: removed {} - {}", fav.name, fav.artist);
                    bot.send_message(
                        chat_id,
                        format!("🗑 已取消收藏：{} - {}", fav.name, fav.artist),
                    )
                    .await?;
                }
                Ok(false) => {}
                Err(e) => {
                    bot.send_message(chat_id, format!("❌ 取消收藏失败：{}", e)).await?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// 歌单查看/点播回调：playlist:show（列出歌单歌曲）/ playlist:play:<idx>（播放）。
async fn handle_playlist_cb(
    bot: Bot,
    ctx: Context,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    data: String,
) -> Result<()> {
    let parts: Vec<&str> = data.split(':').collect();
    match parts.get(1) {
        Some(&"show") => {
            let Some(playlist) = ctx.playlist.lock().clone() else {
                edit_message(&bot, chat_id, msg_id, "❌ 未设置歌单，先用 /playlist 选择或创建").await;
                return Ok(());
            };
            render_playlist_songs(&bot, &ctx, chat_id, msg_id, &playlist).await;
        }
        Some(&"open") => {
            // 从 /playlist 歌单列表点按钮打开：playlist:open:<id>
            let Some(id) = parts.get(2).map(|s| s.to_string()) else {
                return Ok(());
            };
            let Some(emby) = ctx.emby.clone() else {
                edit_message(&bot, chat_id, msg_id, "Emby 联动未启用").await;
                return Ok(());
            };
            let info = match emby.get_playlist(&id).await {
                Ok(i) => i,
                Err(e) => {
                    edit_message(&bot, chat_id, msg_id, format!("❌ 读取歌单失败：{}", e)).await;
                    return Ok(());
                }
            };
            let playlist = PlaylistCtx { id: info.id, name: info.name };
            ctx.playlist.lock().replace(playlist.clone());
            info!(">> EMBY: playlist opened {} ({})", playlist.name, playlist.id);
            render_playlist_songs(&bot, &ctx, chat_id, msg_id, &playlist).await;
        }
        Some(&"play") => {
            let Some(idx) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) else {
                return Ok(());
            };
            let pending = ctx.playlist_pending.lock().get(&chat_id).cloned();
            let Some(pending) = pending else {
                edit_message(&bot, chat_id, msg_id, "❌ 列表已过期，请重新点 📋 查看歌单").await;
                return Ok(());
            };
            if pending.expired() {
                ctx.playlist_pending.lock().remove(&chat_id);
                edit_message(&bot, chat_id, msg_id, "❌ 列表已过期，请重新点 📋 查看歌单").await;
                return Ok(());
            }
            let Some(song) = pending.songs.get(idx).cloned() else {
                return Ok(());
            };
            let path = PathBuf::from(&song.Path);
            if !path.exists() {
                edit_message(
                    &bot,
                    chat_id,
                    msg_id,
                    format!("⚠️ 文件不可见：{}（挂载不一致？）", song.Path),
                )
                .await;
                return Ok(());
            }
            let artist = if song.Artists.is_empty() {
                "未知歌手".to_string()
            } else {
                song.Artists.join("/")
            };
            edit_message(&bot, chat_id, msg_id, format!("📤 正在发送：{} - {}", song.Name, artist)).await;
            info!(">> PLAYLIST: play {} -> {}", song.Name, song.Path);
            let mut req = bot.send_audio(chat_id, InputFile::file(&path));
            req.payload_mut().title = Some(song.Name.clone());
            req.payload_mut().performer = Some(artist);
            req.await?;
        }
        _ => {}
    }
    Ok(())
}

/// 列出歌单内歌曲并附点播序号按钮（playlist:show 与 playlist:open 共用）。
async fn render_playlist_songs(
    bot: &Bot,
    ctx: &Context,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    playlist: &PlaylistCtx,
) {
    let Some(emby) = ctx.emby.clone() else {
        edit_message(bot, chat_id, msg_id, "Emby 联动未启用").await;
        return;
    };
    let songs = match emby.playlist_items(&playlist.id).await {
        Ok(s) => s,
        Err(e) => {
            edit_message(bot, chat_id, msg_id, format!("❌ 读取歌单失败：{}", e)).await;
            return;
        }
    };
    if songs.is_empty() {
        edit_message(
            bot,
            chat_id,
            msg_id,
            format!(
                "📋 歌单「{}」还是空的。用 /music 下载试听后 📥入库，再点 ➕ 加入歌单",
                playlist.name
            ),
        )
        .await;
        return;
    }
    ctx.playlist_pending.lock().insert(
        chat_id,
        PendingEmby {
            songs: songs.clone(),
            created: Instant::now(),
        },
    );
    let mut text = format!(
        "📋 歌单「{}」（{} 首），点序号播放（60 秒内有效）：\n",
        playlist.name,
        songs.len()
    );
    for (i, s) in songs.iter().enumerate() {
        let artist = if s.Artists.is_empty() {
            "未知歌手".to_string()
        } else {
            s.Artists.join("/")
        };
        text.push_str(&format!("{}. {} - {}\n", i + 1, s.Name, artist));
    }
    // 序号按钮横向一排（每行最多 8 个，多了自动换行）
    let buttons: Vec<InlineKeyboardButton> = songs
        .iter()
        .enumerate()
        .map(|(i, _)| {
            InlineKeyboardButton::callback(
                (i + 1).to_string(),
                format!("playlist:play:{}", i),
            )
        })
        .collect();
    let rows: Vec<Vec<InlineKeyboardButton>> =
        buttons.chunks(8).map(|c| c.to_vec()).collect();
    edit_message_with_kb(bot, chat_id, msg_id, text, InlineKeyboardMarkup::new(rows)).await;
}

/// 编辑消息文本（去掉键盘）。
async fn edit_message(
    bot: &Bot,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    text: impl Into<String>,
) {
    let mut req = bot.edit_message_text(chat_id, msg_id, text.into());
    req.payload_mut().reply_markup = None;
    req.await.ok();
}

/// 编辑消息文本 + 内联键盘。
async fn edit_message_with_kb(
    bot: &Bot,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    text: impl Into<String>,
    kb: InlineKeyboardMarkup,
) {
    let mut req = bot.edit_message_text(chat_id, msg_id, text.into());
    req.payload_mut().reply_markup = Some(kb);
    req.await.ok();
}
