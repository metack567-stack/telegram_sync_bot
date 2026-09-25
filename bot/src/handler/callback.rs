use crate::{
    context::Context,
    emby::{EmbySong, PendingEmby, PendingPlaylistList, PlaylistCtx},
    handler::message::{ext_to_br, make_act_from_emby, send_info_card, trial_panel},
    sqm::{MusicRecord, PendingMusic},
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
        && !data.starts_with("music:")
        && !data.starts_with("emby:pick:")
        && !data.starts_with("emby:add:")
        && !data.starts_with("emby:del")
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
/// music:pick:<idx>（选歌）、music:dl:<idx>:<br>（选音质并下载）、
/// music:act:<play|keep|fav|del|playlist>（信息卡面板操作）、music:back（返回选歌）、music:cancel（取消）。
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
        Some(&"online") => handle_music_online(bot, ctx, chat_id, msg_id, &parts).await,
        Some(&"libpick") => handle_music_libpick(bot, ctx, chat_id, msg_id, &parts).await,
        Some(&"pick") => handle_music_pick(bot, ctx, chat_id, msg_id, &parts).await,
        Some(&"dl") => handle_music_dl(bot, ctx, chat_id, msg_id, &parts).await,
        Some(&"act") => handle_music_act(bot, ctx, storage, chat_id, msg_id, &parts).await,
        Some(&"back") => handle_music_back(bot, ctx, chat_id, msg_id).await,
        Some(&"cancel") => handle_music_cancel(bot, ctx, chat_id, msg_id).await,
        _ => Ok(()),
    }
}

/// 渲染 /music 搜索结果面板（序号按钮横向一排）。
/// msg_id 为 None 时发新消息（/music 命令），否则编辑现有消息（音质面板 🔙 返回用）。
pub(crate) async fn render_music_results(
    bot: &Bot,
    ctx: &Context,
    chat_id: ChatId,
    msg_id: Option<teloxide::types::MessageId>,
    keyword: &str,
    songs: &[MusicRecord],
    pref: Option<&str>,
) {
    let top = songs.iter().take(5).cloned().collect::<Vec<_>>();
    ctx.music_pending.lock().insert(
        chat_id,
        PendingMusic {
            songs: top.clone(),
            created: Instant::now(),
            pref: pref.map(|s| s.to_string()),
            chosen: None,
            keyword: keyword.to_string(),
        },
    );
    let mut text = format!(
        "🎵 搜索到「{}」相关歌曲，点下方序号选择（60 秒内有效）：\n",
        keyword
    );
    for (i, s) in top.iter().enumerate() {
        let artist = if s.artistName.is_empty() {
            "未知歌手".to_string()
        } else {
            s.artistName.join("/")
        };
        let album = s.albumName.clone().unwrap_or_else(|| "未知专辑".to_string());
        let br = crate::sqm::pick_br_type_with_pref(&s.brTypes, pref)
            .map(|b| b.replace('_', " "))
            .unwrap_or_else(|| "自动".to_string());
        text.push_str(&format!(
            "{}. {} - {}《{}》〔{}〕\n",
            i + 1,
            s.name,
            artist,
            album,
            br
        ));
    }
    let rows: Vec<Vec<InlineKeyboardButton>> = vec![top
        .iter()
        .enumerate()
        .map(|(i, _)| {
            InlineKeyboardButton::callback((i + 1).to_string(), format!("music:pick:{}", i))
        })
        .collect()];
    text.push_str(
        "\n下载试听后点 📥 入库，即可在 Emby 播放/加入歌单（先 /playlist <歌单名> 创建）",
    );
    let kb = InlineKeyboardMarkup::new(rows);
    match msg_id {
        Some(id) => edit_message_with_kb(bot, chat_id, id, text, kb).await,
        None => {
            let mut req = bot.send_message(chat_id, text);
            req.payload_mut().reply_markup =
                Some(teloxide::types::ReplyMarkup::InlineKeyboard(kb));
            if let Err(e) = req.await {
                warn!(">> MUSIC: send results failed: {}", e);
            }
        }
    }
}

/// 渲染音乐库候选列表（/music 先查 Emby 命中时）：序号横排 + 🔍 在线找歌 + ❌ 取消。
/// 点序号 music:libpick:<idx> 进入管理态信息卡；在线找歌 music:online:<kw> 转 sqmusic 下载流程。
/// msg_id 为 None 时发新消息（/music 命令），否则编辑现有消息（信息卡 🔙 返回用）。
pub(crate) async fn render_library_results(
    bot: &Bot,
    ctx: &Context,
    chat_id: ChatId,
    msg_id: Option<teloxide::types::MessageId>,
    keyword: &str,
    songs: &[EmbySong],
) {
    let top = songs.iter().take(8).cloned().collect::<Vec<_>>();
    if top.is_empty() {
        let text = "音乐库没有相关歌曲";
        match msg_id {
            Some(id) => edit_message(bot, chat_id, id, text).await,
            None => {
                bot.send_message(chat_id, text).await.ok();
            }
        }
        return;
    }
    ctx.emby_pending.lock().insert(
        chat_id,
        PendingEmby {
            songs: top.clone(),
            created: Instant::now(),
            keyword: keyword.to_string(),
        },
    );
    let mut text = format!("🎵 音乐库找到「{}」相关歌曲（点序号管理，60 秒内有效）：\n", keyword);
    for (i, song) in top.iter().enumerate() {
        let artist = if song.Artists.is_empty() {
            "未知歌手".to_string()
        } else {
            song.Artists.join("/")
        };
        let album = song.Album.clone().unwrap_or_else(|| "未知专辑".to_string());
        let br = ext_to_br(&PathBuf::from(&song.Path));
        text.push_str(&format!(
            "{}. {} - {}《{}》〔{}〕\n",
            i + 1,
            song.Name,
            artist,
            album,
            br
        ));
    }
    text.push_str("\n没有想要的？点 🔍 在线找歌从全网搜索下载");
    // 在线找歌按钮回调带上关键词（截断保证 ≤64 字节：13 字节前缀 + 最多 16 个汉字）
    let kw_short: String = keyword.chars().take(16).collect();
    let mut rows: Vec<Vec<InlineKeyboardButton>> = vec![top
        .iter()
        .enumerate()
        .map(|(i, _)| {
            InlineKeyboardButton::callback((i + 1).to_string(), format!("music:libpick:{}", i))
        })
        .collect()];
    rows.push(vec![
        InlineKeyboardButton::callback("🔍 在线找歌", format!("music:online:{}", kw_short)),
        InlineKeyboardButton::callback("❌ 取消", "music:cancel"),
    ]);
    let kb = InlineKeyboardMarkup::new(rows);
    match msg_id {
        Some(id) => edit_message_with_kb(bot, chat_id, id, text, kb).await,
        None => {
            let mut req = bot.send_message(chat_id, text);
            req.payload_mut().reply_markup =
                Some(teloxide::types::ReplyMarkup::InlineKeyboard(kb));
            if let Err(e) = req.await {
                warn!(">> MUSIC: send library results failed: {}", e);
            }
        }
    }
}

/// music:online：从"库内没有"面板转 sqmusic 在线搜索（编辑当前消息为在线候选列表）。
async fn handle_music_online(
    bot: Bot,
    ctx: Context,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    parts: &[&str],
) -> Result<()> {
    let keyword = parts.get(2).unwrap_or(&"").to_string();
    if keyword.is_empty() {
        edit_message(&bot, chat_id, msg_id, "❌ 关键词丢失，请重新 /music 搜索").await;
        return Ok(());
    }
    let Some(sqm) = &ctx.sqmusic else {
        edit_message(&bot, chat_id, msg_id, "❌ sqmusic 联动未启用（未配置 SQMUSIC_URL）").await;
        return Ok(());
    };
    edit_message(&bot, chat_id, msg_id, format!("🔍 正在在线搜索「{}」...", keyword)).await;
    let songs = match sqm.search("kw", &keyword, 5).await {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => {
            edit_message(&bot, chat_id, msg_id, format!("❌ 在线未找到「{}」相关歌曲", keyword))
                .await;
            return Ok(());
        }
        Err(e) => {
            warn!(">> SQMUSIC: search failed: {}", e);
            edit_message(&bot, chat_id, msg_id, format!("❌ 搜索失败：{}", e)).await;
            return Ok(());
        }
    };
    let top = songs.into_iter().take(5).collect::<Vec<_>>();
    render_music_results(&bot, &ctx, chat_id, Some(msg_id), &keyword, &top, None).await;
    Ok(())
}

/// music:libpick:<idx>：选中库内候选 → 管理态信息卡（试听/收藏/删除/加歌单，不自动发文件）。
async fn handle_music_libpick(
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
        edit_message(&bot, chat_id, msg_id, "❌ 选择已过期，请重新 /music 搜索").await;
        return Ok(());
    };
    if pending.expired() {
        ctx.emby_pending.lock().remove(&chat_id);
        edit_message(&bot, chat_id, msg_id, "❌ 选择已过期，请重新 /music 搜索").await;
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
    let music_dir = ctx
        .music_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("/music"));
    let act = make_act_from_emby(&music_dir, &song);
    ctx.music_act.lock().insert(chat_id, act.clone());
    info!(">> MUSIC: library pick {} -> {}", song.Name, path.display());
    let (text, kb) = trial_panel(&act);
    let cover = ctx.emby.as_ref().map(|e| e.cover_url(&song.Id));
    send_info_card(&bot, chat_id, cover.as_deref(), Some(&path), text, kb).await;
    Ok(())
}

/// music:back：从音质选择面板返回歌曲列表（重建 /music 面板）。
async fn handle_music_back(
    bot: Bot,
    ctx: Context,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
) -> Result<()> {
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
    render_music_results(
        &bot,
        &ctx,
        chat_id,
        Some(msg_id),
        &pending.keyword,
        &pending.songs,
        pending.pref.as_deref(),
    )
    .await;
    Ok(())
}

/// music:cancel：取消本次选歌。
async fn handle_music_cancel(
    bot: Bot,
    ctx: Context,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
) -> Result<()> {
    ctx.music_pending.lock().remove(&chat_id);
    edit_message(&bot, chat_id, msg_id, "已取消").await;
    Ok(())
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

/// 信息卡操作面板：music:act:<play|keep|fav|del|playlist>。
/// play = 发送试听文件；keep = 移入音乐库（copy + 删临时）+ Emby 刷新；fav = 写收藏（文件保留）；
/// del = 删临时文件（仅 is_tmp 文件）；playlist = 入库后把歌加入当前 Emby 歌单。
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
        warn!(">> MUSIC: act {} but no pending for chat {}", act, chat_id);
        edit_message(&bot, chat_id, msg_id, "❌ 操作已过期，请重新 /music 下载试听").await;
        return Ok(());
    };
    info!(">> MUSIC: act {} for {} (is_tmp={})", act, pending.name, pending.is_tmp);
    if pending.expired() {
        ctx.music_act.lock().remove(&chat_id);
        edit_message(&bot, chat_id, msg_id, "❌ 操作已过期（10 分钟），请重新 /music 下载试听").await;
        return Ok(());
    }
    match act.as_str() {
        "play" => {
            // ▶️ 试听：把试听文件发回（保留操作面板，试听完可继续 入库/收藏/删除）
            let path = PathBuf::from(&pending.tmp_path);
            if !path.exists() {
                edit_message(
                    &bot,
                    chat_id,
                    msg_id,
                    format!("⚠️ 试听文件不存在：{}（可能已被清理）", pending.name),
                )
                .await;
                return Ok(());
            }
            let kb = if pending.is_tmp {
                InlineKeyboardMarkup::new(vec![
                    vec![InlineKeyboardButton::callback("▶️ 试听", "music:act:play")],
                    vec![
                        InlineKeyboardButton::callback("📥 入库", "music:act:keep"),
                        InlineKeyboardButton::callback("❤️ 收藏", "music:act:fav"),
                        InlineKeyboardButton::callback("🗑 删除", "music:act:del"),
                    ],
                ])
            } else {
                InlineKeyboardMarkup::new(vec![vec![
                    InlineKeyboardButton::callback("▶️ 试听", "music:act:play"),
                ]])
            };
            edit_message_with_kb(
                &bot,
                chat_id,
                msg_id,
                format!("📤 正在发送试听：{} - {}", pending.name, pending.artist),
                kb,
            )
            .await;
            info!(">> MUSIC: preview {} -> {}", pending.name, path.display());
            let mut req = bot.send_audio(chat_id, InputFile::file(&path));
            req.payload_mut().title = Some(pending.name.clone());
            req.payload_mut().performer = Some(pending.artist.clone());
            req.await?;
        }
        "keep" => {
            if !pending.is_tmp {
                edit_message(&bot, chat_id, msg_id, "✅ 该歌已在音乐库中，无需再次入库").await;
                return Ok(());
            }
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
                    warn!(">> MUSIC: keep failed: {}", e);
                    edit_message(&bot, chat_id, msg_id, format!("❌ 入库失败：{}", e)).await;
                }
            }
        }
        "fav" => {
            let album = pending.album.clone().unwrap_or_default();
            match storage.add_favorite(chat_id, &pending.name, &pending.artist, &album).await {
                Ok(true) => {
                    info!(">> FAVS: added {} - {}", pending.name, pending.artist);
                    if pending.kept {
                        // 已入库后才收藏：临时文件已删除，回到"已入库"面板
                        let text = format!(
                            "❤️ 已收藏：{} - {}\n（该歌已在音乐库，可加入歌单）",
                            pending.name, pending.artist
                        );
                        let kb = InlineKeyboardMarkup::new(vec![vec![
                            InlineKeyboardButton::callback("➕ 加入歌单", "music:act:playlist"),
                            InlineKeyboardButton::callback("❤️ 已收藏", "music:act:fav"),
                        ]]);
                        edit_message_with_kb(&bot, chat_id, msg_id, text, kb).await;
                    } else {
                        let text = format!(
                            "❤️ 已收藏：{} - {}\n（试听文件保留，可继续 ▶️试听 / 📥入库 / 🗑删除）",
                            pending.name, pending.artist
                        );
                        let kb = InlineKeyboardMarkup::new(vec![
                            vec![InlineKeyboardButton::callback("▶️ 试听", "music:act:play")],
                            vec![
                                InlineKeyboardButton::callback("📥 入库", "music:act:keep"),
                                InlineKeyboardButton::callback("🗑 删除", "music:act:del"),
                            ],
                        ]);
                        edit_message_with_kb(&bot, chat_id, msg_id, text, kb).await;
                    }
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
                    warn!(">> FAVS: add failed: {}", e);
                    edit_message(&bot, chat_id, msg_id, format!("❌ 收藏失败：{}", e)).await;
                }
            }
        }
        "del" => {
            if !pending.is_tmp {
                if pending.emby_id.is_some() {
                    // 音乐库文件：两步确认（删除会连同文件一起删，不可恢复）
                    let text = format!(
                        "⚠️ 确认删除「{} - {}」？\n这会连同音乐库文件一起删除，不可恢复。",
                        pending.name, pending.artist
                    );
                    let kb = InlineKeyboardMarkup::new(vec![vec![
                        InlineKeyboardButton::callback("🗑 确认删除", "music:act:delconf"),
                        InlineKeyboardButton::callback("🔙 取消", "music:act:delback"),
                    ]]);
                    edit_message_with_kb(&bot, chat_id, msg_id, text, kb).await;
                    return Ok(());
                }
                edit_message(
                    &bot,
                    chat_id,
                    msg_id,
                    format!(
                        "⚠️ 「{}」在音乐库中（非临时文件）。删除已入库歌曲请用 /emby 搜索后点 🗑（会连同文件删除）",
                        pending.name
                    ),
                )
                .await;
                return Ok(());
            }
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
        "delconf" => {
            // 确认删除音乐库歌曲（Emby DELETE，连同文件删除）
            let Some(id) = pending.emby_id.clone() else {
                edit_message(&bot, chat_id, msg_id, "⚠️ 该歌没有关联 Emby 记录，请用 /emby 搜索后删除").await;
                return Ok(());
            };
            let Some(emby) = ctx.emby.clone() else {
                edit_message(&bot, chat_id, msg_id, "❌ Emby 未启用").await;
                return Ok(());
            };
            match emby.delete_item(&id).await {
                Ok(_) => {
                    ctx.music_act.lock().remove(&chat_id);
                    info!(">> MUSIC: deleted library {} ({})", pending.name, id);
                    edit_message(
                        &bot,
                        chat_id,
                        msg_id,
                        format!("🗑 已删除「{}」及其文件（Emby 已同步）", pending.name),
                    )
                    .await;
                }
                Err(e) => {
                    warn!(">> MUSIC: delete library item failed: {}", e);
                    edit_message(&bot, chat_id, msg_id, format!("❌ 删除失败：{}", e)).await;
                }
            }
        }
        "delback" => {
            // 取消删除：重渲染原信息卡面板
            let (text, kb) = crate::handler::message::trial_panel(&pending);
            edit_message_with_kb(&bot, chat_id, msg_id, text, kb).await;
        }
        "playlist" => {
            if !pending.kept {
                edit_message(&bot, chat_id, msg_id, "⚠️ 请先 📥 入库，才能加入歌单").await;
                return Ok(());
            }
            // 弹出 Emby 歌单列表，点序号当场选择要加入的歌单
            render_playlist_list_ui(&bot, &ctx, chat_id, Some(msg_id), PlaylistListMode::AddTo).await;
        }
        "back" => {
            // 管理态（库内命中）：回库内候选列表；否则回在线候选列表
            // 注意：MutexGuard 必须语句结束即 drop（拆出 let），不能留在 if-let 临时值里跨 await
            if !pending.is_tmp {
                let lib_pending = ctx.emby_pending.lock().get(&chat_id).cloned();
                if let Some(p) = lib_pending {
                    if !p.expired() {
                        render_library_results(&bot, &ctx, chat_id, Some(msg_id), &p.keyword, &p.songs)
                            .await;
                        return Ok(());
                    }
                }
            }
            handle_music_back(bot, ctx, chat_id, msg_id).await?
        }
        _ => {}
    }
    Ok(())
}

/// Emby 点播回调入口：emby:pick:<idx> / emby:add:<idx> / emby:del（删除）系列。
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
        Some(&"del") => handle_emby_del(bot, ctx, chat_id, msg_id, &parts).await,
        Some(&"delpick") => handle_emby_delpick(bot, ctx, chat_id, msg_id, &parts).await,
        Some(&"delconf") => handle_emby_delconf(bot, ctx, chat_id, msg_id, &parts).await,
        Some(&"delback") => handle_emby_delback(bot, ctx, chat_id, msg_id, &parts).await,
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

/// 渲染 /emby 搜索结果面板（序号播放 + ➕ 加歌单 + 🗑 删除）。
/// msg_id 为 None 时发新消息（/emby 命令），否则编辑现有消息（删除后返回用）。
/// keyword 为 None 时用通用标题（返回场景拿不到关键词）。
pub(crate) async fn render_emby_results(
    bot: &Bot,
    ctx: &Context,
    chat_id: ChatId,
    msg_id: Option<teloxide::types::MessageId>,
    keyword: Option<&str>,
    songs: &[EmbySong],
) {
    let top = songs.iter().take(8).cloned().collect::<Vec<_>>();
    if top.is_empty() {
        let text = "Emby 音乐库没有相关歌曲";
        match msg_id {
            Some(id) => edit_message(bot, chat_id, id, text).await,
            None => {
                let _ = bot.send_message(chat_id, text).await;
            }
        }
        return;
    }
    ctx.emby_pending.lock().insert(
        chat_id,
        PendingEmby {
            songs: top.clone(),
            created: Instant::now(),
            keyword: keyword.unwrap_or_default().to_string(),
        },
    );
    let mut text = match keyword {
        Some(k) => format!("🎵 Emby 音乐库「{}」相关，点下方序号发送（60 秒内有效）：\n", k),
        None => "🎵 Emby 音乐库搜索结果，点下方序号发送（60 秒内有效）：\n".to_string(),
    };
    for (i, s) in top.iter().enumerate() {
        let artist = if s.Artists.is_empty() {
            "未知歌手".to_string()
        } else {
            s.Artists.join("/")
        };
        let album = s.Album.clone().unwrap_or_else(|| "未知专辑".to_string());
        text.push_str(&format!("{}. {} - {}《{}》\n", i + 1, s.Name, artist, album));
    }
    // 第一行：序号按钮横向一排，点一下直接发送该歌
    let mut rows: Vec<Vec<InlineKeyboardButton>> = vec![top
        .iter()
        .enumerate()
        .map(|(i, _)| {
            InlineKeyboardButton::callback((i + 1).to_string(), format!("emby:pick:{}", i))
        })
        .collect()];
    // 第二行：➕ 把歌加入当前 Emby 歌单（已设置歌单时显示）
    if let Some(p) = ctx.playlist.lock().clone() {
        rows.push(
            top.iter()
                .enumerate()
                .map(|(i, _)| {
                    InlineKeyboardButton::callback(format!("➕{}", i + 1), format!("emby:add:{}", i))
                })
                .collect(),
        );
        text.push_str(&format!("\n点 ➕ 把歌加入歌单「{}」", p.name));
    } else {
        text.push_str("\n（用 /playlist <歌单名> 创建歌单后，可一键把歌加入 Emby 歌单）");
    }
    // 第三行：🗑 从音乐库删除歌曲（两步确认）
    rows.push(vec![InlineKeyboardButton::callback("🗑 删除歌曲", "emby:del")]);
    text.push_str("\n点 🗑 删除音乐库中的歌曲（两步确认，会同时删除文件）");
    let kb = InlineKeyboardMarkup::new(rows);
    match msg_id {
        Some(id) => edit_message_with_kb(bot, chat_id, id, text, kb).await,
        None => {
            let mut req = bot.send_message(chat_id, text);
            req.payload_mut().reply_markup =
                Some(teloxide::types::ReplyMarkup::InlineKeyboard(kb));
            if let Err(e) = req.await {
                warn!(">> EMBY: send results failed: {}", e);
            }
        }
    }
}

/// 🗑 删除歌曲：emby:del（进入序号选择，复用当前 /emby 搜索结果）。
async fn handle_emby_del(
    bot: Bot,
    ctx: Context,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    _parts: &[&str],
) -> Result<()> {
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
    let songs = pending.songs.clone();
    ctx.emby_del_pending.lock().insert(
        chat_id,
        PendingEmby {
            songs: songs.clone(),
            created: Instant::now(),
            keyword: String::new(),
        },
    );
    let mut text = format!(
        "🗑 选择要删除的歌曲（共 {} 首，点序号两步确认，会同时删除文件）：\n",
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
    let buttons: Vec<InlineKeyboardButton> = songs
        .iter()
        .enumerate()
        .map(|(i, _)| {
            InlineKeyboardButton::callback((i + 1).to_string(), format!("emby:delpick:{}", i))
        })
        .collect();
    let mut rows: Vec<Vec<InlineKeyboardButton>> =
        buttons.chunks(8).map(|c| c.to_vec()).collect();
    rows.push(vec![InlineKeyboardButton::callback("🔙 返回", "emby:delback")]);
    edit_message_with_kb(&bot, chat_id, msg_id, text, InlineKeyboardMarkup::new(rows)).await;
    Ok(())
}

/// emby:delpick:<idx>：点序号进入两步确认。
async fn handle_emby_delpick(
    bot: Bot,
    ctx: Context,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    parts: &[&str],
) -> Result<()> {
    let Some(idx) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) else {
        return Ok(());
    };
    let pending = ctx.emby_del_pending.lock().get(&chat_id).cloned();
    let Some(pending) = pending else {
        edit_message(&bot, chat_id, msg_id, "❌ 选择已过期，请重新 /emby 搜索").await;
        return Ok(());
    };
    if pending.expired() {
        ctx.emby_del_pending.lock().remove(&chat_id);
        edit_message(&bot, chat_id, msg_id, "❌ 选择已过期，请重新 /emby 搜索").await;
        return Ok(());
    }
    let Some(song) = pending.songs.get(idx).cloned() else {
        return Ok(());
    };
    let artist = if song.Artists.is_empty() {
        "未知歌手".to_string()
    } else {
        song.Artists.join("/")
    };
    let text = format!(
        "⚠️ 确认删除「{} - {}」？\n会同时删除音乐文件，不可恢复。",
        song.Name, artist
    );
    let kb = InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            "✅ 确认删除",
            format!("emby:delconf:{}", idx),
        )],
        vec![InlineKeyboardButton::callback("❌ 取消", "emby:delback")],
    ]);
    edit_message_with_kb(&bot, chat_id, msg_id, text, kb).await;
    Ok(())
}

/// emby:delconf:<idx>：确认删除（DELETE /Items/{id} + 刷新音乐库）。
async fn handle_emby_delconf(
    bot: Bot,
    ctx: Context,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    parts: &[&str],
) -> Result<()> {
    let Some(idx) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) else {
        return Ok(());
    };
    let pending = ctx.emby_del_pending.lock().get(&chat_id).cloned();
    let Some(pending) = pending else {
        edit_message(&bot, chat_id, msg_id, "❌ 选择已过期，请重新 /emby 搜索").await;
        return Ok(());
    };
    if pending.expired() {
        ctx.emby_del_pending.lock().remove(&chat_id);
        edit_message(&bot, chat_id, msg_id, "❌ 选择已过期，请重新 /emby 搜索").await;
        return Ok(());
    }
    let Some(song) = pending.songs.get(idx).cloned() else {
        return Ok(());
    };
    let Some(emby) = ctx.emby.clone() else {
        return Ok(());
    };
    edit_message(&bot, chat_id, msg_id, format!("🗑 正在删除：{}", song.Name)).await;
    match emby.delete_item(&song.Id).await {
        Ok(()) => {
            // 从删除候选与搜索结果里移除该歌（guard 全部临时，避免跨 await 捕获非 Send）
            if let Some(p) = ctx.emby_del_pending.lock().get_mut(&chat_id) {
                p.songs.retain(|x| x.Id != song.Id);
            }
            if let Some(p) = ctx.emby_pending.lock().get_mut(&chat_id) {
                p.songs.retain(|x| x.Id != song.Id);
            }
            if let Err(e) = emby.refresh_library().await {
                warn!(">> EMBY: refresh after delete failed: {}", e);
            }
            info!(">> EMBY: deleted item {} ({})", song.Name, song.Id);
            edit_message(
                &bot,
                chat_id,
                msg_id,
                format!("🗑 已删除：{}（音乐库已刷新）", song.Name),
            )
            .await;
        }
        Err(e) => {
            warn!(">> EMBY: delete item failed: {}", e);
            edit_message(&bot, chat_id, msg_id, format!("❌ 删除失败：{}", e)).await;
        }
    }
    Ok(())
}

/// emby:delback：返回 /emby 搜索结果面板（或取消）。
async fn handle_emby_delback(
    bot: Bot,
    ctx: Context,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    _parts: &[&str],
) -> Result<()> {
    let pending = ctx.emby_pending.lock().get(&chat_id).cloned();
    match pending {
        Some(p) if !p.expired() => {
            render_emby_results(&bot, &ctx, chat_id, Some(msg_id), None, &p.songs).await;
        }
        _ => {
            edit_message(&bot, chat_id, msg_id, "已取消").await;
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
    match parts.get(1) {
        Some(&"back") => {
            render_favs_results(&bot, chat_id, Some(msg_id), &storage).await;
        }
        Some(&"play") => {
            let Some(idx) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) else {
                return Ok(());
            };
            let favs = storage.list_favorites(chat_id).await?;
            let Some(fav) = favs.get(idx).cloned() else {
                return Ok(());
            };            let Some(emby) = ctx.emby.clone() else {
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
                    // 附"返回收藏列表"与"取消收藏"操作
                    let kb = InlineKeyboardMarkup::new(vec![vec![
                        InlineKeyboardButton::callback("🔙 返回收藏列表", "favs:back"),
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
            let Some(idx) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) else {
                return Ok(());
            };
            let favs = storage.list_favorites(chat_id).await?;
            let Some(fav) = favs.get(idx).cloned() else {
                return Ok(());
            };
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

/// 渲染收藏列表（序号按钮横向一排）。
/// msg_id 为 None 时发新消息（/favs 命令），否则编辑现有消息（播放后 🔙 返回用）。
pub(crate) async fn render_favs_results(
    bot: &Bot,
    chat_id: ChatId,
    msg_id: Option<teloxide::types::MessageId>,
    storage: &MyStorage,
) {
    let favs = match storage.list_favorites(chat_id).await {
        Ok(f) => f,
        Err(e) => {
            let text = format!("❌ 读取收藏失败：{}", e);
            match msg_id {
                Some(id) => edit_message(bot, chat_id, id, text).await,
                None => {
                    let _ = bot.send_message(chat_id, text).await;
                }
            }
            return;
        }
    };
    if favs.is_empty() {
        let text = "🎵 还没有收藏。用 /music 下载试听后点 ❤️ 收藏";
        match msg_id {
            Some(id) => edit_message(bot, chat_id, id, text).await,
            None => {
                let _ = bot.send_message(chat_id, text).await;
            }
        }
        return;
    }
    let mut text = format!("🎵 我的收藏（{}），点序号播放：\n", favs.len());
    for (i, f) in favs.iter().enumerate() {
        let album = if f.album.is_empty() {
            String::new()
        } else {
            format!("《{}》", f.album)
        };
        text.push_str(&format!(
            "{}. {} - {}{}（收藏于 {}）\n",
            i + 1, f.name, f.artist, album, f.created_at
        ));
    }
    let buttons: Vec<InlineKeyboardButton> = favs
        .iter()
        .enumerate()
        .map(|(i, _)| {
            InlineKeyboardButton::callback((i + 1).to_string(), format!("favs:play:{}", i))
        })
        .collect();
    let rows: Vec<Vec<InlineKeyboardButton>> = buttons.chunks(8).map(|c| c.to_vec()).collect();
    let kb = InlineKeyboardMarkup::new(rows);
    match msg_id {
        Some(id) => edit_message_with_kb(bot, chat_id, id, text, kb).await,
        None => {
            let mut req = bot.send_message(chat_id, text);
            req.payload_mut().reply_markup =
                Some(teloxide::types::ReplyMarkup::InlineKeyboard(kb));
            if let Err(e) = req.await {
                warn!(">> FAVS: send list failed: {}", e);
            }
        }
    }
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
            // 从 /playlist 歌单列表点序号打开：playlist:open:<idx>
            let Some(idx) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) else {
                return Ok(());
            };
            let pending = ctx.playlist_list_pending.lock().get(&chat_id).cloned();
            let Some(pending) = pending else {
                edit_message(&bot, chat_id, msg_id, "❌ 歌单列表已过期，请重新 /playlist").await;
                return Ok(());
            };
            if pending.expired() {
                ctx.playlist_list_pending.lock().remove(&chat_id);
                edit_message(&bot, chat_id, msg_id, "❌ 歌单列表已过期，请重新 /playlist").await;
                return Ok(());
            }
            let Some(info) = pending.lists.get(idx).cloned() else {
                return Ok(());
            };
            let playlist = PlaylistCtx { id: info.id, name: info.name };
            ctx.playlist.lock().replace(playlist.clone());
            info!(">> EMBY: playlist opened {} ({})", playlist.name, playlist.id);
            render_playlist_songs(&bot, &ctx, chat_id, msg_id, &playlist).await;
        }
        Some(&"list") => {
            // 从歌单歌曲列表点 🔙 返回：重新显示歌单列表
            render_playlist_list_ui(&bot, &ctx, chat_id, Some(msg_id), PlaylistListMode::Open).await;
        }
        Some(&"dellist") => {
            // 进入删除模式：选择要删除的歌单
            render_playlist_list_ui(&bot, &ctx, chat_id, Some(msg_id), PlaylistListMode::Delete).await;
        }
        Some(&"delpick") => {
            // 删除模式点序号：两步确认
            let Some(idx) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) else {
                return Ok(());
            };
            let pending = ctx.playlist_list_pending.lock().get(&chat_id).cloned();
            let Some(pending) = pending else {
                edit_message(&bot, chat_id, msg_id, "❌ 歌单列表已过期，请重新 /playlist").await;
                return Ok(());
            };
            if pending.expired() {
                ctx.playlist_list_pending.lock().remove(&chat_id);
                edit_message(&bot, chat_id, msg_id, "❌ 歌单列表已过期，请重新 /playlist").await;
                return Ok(());
            }
            let Some(info) = pending.lists.get(idx).cloned() else {
                return Ok(());
            };
            let text = format!("⚠️ 确认删除歌单「{}」？删除后不可恢复。", info.name);
            let kb = InlineKeyboardMarkup::new(vec![
                vec![InlineKeyboardButton::callback(
                    "✅ 确认删除",
                    format!("playlist:deldone:{}", idx),
                )],
                vec![InlineKeyboardButton::callback("❌ 取消", "playlist:list")],
            ]);
            edit_message_with_kb(&bot, chat_id, msg_id, text, kb).await;
        }
        Some(&"deldone") => {
            // 确认删除歌单
            let Some(idx) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) else {
                return Ok(());
            };
            let pending = ctx.playlist_list_pending.lock().get(&chat_id).cloned();
            let Some(pending) = pending else {
                edit_message(&bot, chat_id, msg_id, "❌ 歌单列表已过期，请重新 /playlist").await;
                return Ok(());
            };
            let Some(info) = pending.lists.get(idx).cloned() else {
                return Ok(());
            };
            let Some(emby) = ctx.emby.clone() else {
                return Ok(());
            };
            match emby.delete_playlist(&info.id).await {
                Ok(()) => {
                    // 若删除的是当前歌单，清除选中（guard 全部临时，避免跨 await 捕获非 Send）
                    if ctx.playlist.lock().as_ref().is_some_and(|c| c.id == info.id) {
                        *ctx.playlist.lock() = None;
                    }
                    // 从暂存列表移除该歌单
                    if let Some(p) = ctx.playlist_list_pending.lock().get_mut(&chat_id) {
                        p.lists.retain(|x| x.id != info.id);
                    }
                    info!(">> EMBY: playlist deleted {} ({})", info.name, info.id);
                    edit_message(&bot, chat_id, msg_id, format!("🗑 已删除歌单「{}」", info.name)).await;
                }
                Err(e) => {
                    edit_message(&bot, chat_id, msg_id, format!("❌ 删除歌单失败：{}", e)).await;
                }
            }
        }
        Some(&"addpick") => {
            // ➕ 加入歌单：点序号加入对应歌单
            let Some(idx) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) else {
                return Ok(());
            };
            let pending_list = ctx.playlist_list_pending.lock().get(&chat_id).cloned();
            let Some(pending_list) = pending_list else {
                edit_message(&bot, chat_id, msg_id, "❌ 歌单列表已过期，请重新操作").await;
                return Ok(());
            };
            if pending_list.expired() {
                ctx.playlist_list_pending.lock().remove(&chat_id);
                edit_message(&bot, chat_id, msg_id, "❌ 歌单列表已过期，请重新操作").await;
                return Ok(());
            }
            let Some(info) = pending_list.lists.get(idx).cloned() else {
                return Ok(());
            };
            let pending = ctx.music_act.lock().get(&chat_id).cloned();
            let Some(pending) = pending else {
                edit_message(&bot, chat_id, msg_id, "❌ 操作已过期，请重新 /music 下载试听").await;
                return Ok(());
            };
            if pending.expired() {
                ctx.music_act.lock().remove(&chat_id);
                edit_message(&bot, chat_id, msg_id, "❌ 操作已过期，请重新 /music 下载试听").await;
                return Ok(());
            }
            if !pending.kept {
                edit_message(&bot, chat_id, msg_id, "⚠️ 请先 📥 入库，才能加入歌单").await;
                return Ok(());
            }
            let Some(emby) = ctx.emby.clone() else {
                return Ok(());
            };
            let artist = if pending.artist == "未知歌手" {
                None
            } else {
                Some(pending.artist.as_str())
            };
            match emby.find_song(&pending.name, artist).await {
                Ok(Some(found)) => match emby.add_to_playlist(&info.id, &found.Id).await {
                    Ok(()) => {
                        // 记住最后使用的歌单
                        ctx.playlist.lock().replace(PlaylistCtx {
                            id: info.id.clone(),
                            name: info.name.clone(),
                        });
                        info!(">> EMBY: add {} to playlist {} ({})", found.Name, info.name, info.id);
                        edit_message(
                            &bot,
                            chat_id,
                            msg_id,
                            format!("➕ 已加入歌单「{}」：{}", info.name, found.Name),
                        )
                        .await;
                    }
                    Err(e) => {
                        warn!(">> EMBY: add to playlist failed: {}", e);
                        edit_message(&bot, chat_id, msg_id, format!("❌ 加入歌单失败：{}", e)).await;
                    }
                },
                Ok(None) => {
                    edit_message(
                        &bot,
                        chat_id,
                        msg_id,
                        format!(
                            "⚠️ 音乐库还没扫到「{}」，稍等几分钟再试（或 /emby {} 确认）",
                            pending.name, pending.name
                        ),
                    )
                    .await;
                }
                Err(e) => {
                    edit_message(&bot, chat_id, msg_id, format!("❌ 查询音乐库失败：{}", e)).await;
                }
            }
        }
        Some(&"actback") => {
            // ➕ 加入歌单弹列表后 🔙 返回：重建入库面板
            let pending = ctx.music_act.lock().get(&chat_id).cloned();
            let Some(pending) = pending else {
                edit_message(&bot, chat_id, msg_id, "❌ 操作已过期，请重新 /music 下载试听").await;
                return Ok(());
            };
            if pending.expired() {
                ctx.music_act.lock().remove(&chat_id);
                edit_message(&bot, chat_id, msg_id, "❌ 操作已过期，请重新 /music 下载试听").await;
                return Ok(());
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
        Some(&"act") => {
            // 歌单歌曲点序号：先弹操作面板（播放/删除），不直接发文件
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
            let artist = if song.Artists.is_empty() {
                "未知歌手".to_string()
            } else {
                song.Artists.join("/")
            };
            let text = format!("🎵 {} - {}\n请选择操作：", song.Name, artist);
            let kb = InlineKeyboardMarkup::new(vec![
                vec![
                    InlineKeyboardButton::callback("▶️ 播放", format!("playlist:play:{}", idx)),
                    InlineKeyboardButton::callback("🗑 删除", format!("playlist:songdel:{}", idx)),
                ],
                vec![InlineKeyboardButton::callback("🔙 返回歌单歌曲列表", "playlist:songback")],
            ]);
            edit_message_with_kb(&bot, chat_id, msg_id, text, kb).await;
        }
        Some(&"songdel") => {
            // 歌单内删除歌曲：两步确认
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
            let artist = if song.Artists.is_empty() {
                "未知歌手".to_string()
            } else {
                song.Artists.join("/")
            };
            let text = format!(
                "⚠️ 确认从音乐库删除「{} - {}」？\n会同时删除音乐文件，不可恢复，歌单将自动移除该歌。",
                song.Name, artist
            );
            let kb = InlineKeyboardMarkup::new(vec![
                vec![InlineKeyboardButton::callback(
                    "✅ 确认删除",
                    format!("playlist:songdelconf:{}", idx),
                )],
                vec![InlineKeyboardButton::callback("❌ 取消", "playlist:songback")],
            ]);
            edit_message_with_kb(&bot, chat_id, msg_id, text, kb).await;
        }
        Some(&"songdelconf") => {
            // 确认：从音乐库删除该歌（DELETE /Items/{id} + 刷新）
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
            let Some(emby) = ctx.emby.clone() else {
                return Ok(());
            };
            edit_message(&bot, chat_id, msg_id, format!("🗑 正在删除：{}", song.Name)).await;
            match emby.delete_item(&song.Id).await {
                Ok(()) => {
                    // 从歌单歌曲列表移除该歌（guard 全部临时，避免跨 await 捕获非 Send）
                    if let Some(p) = ctx.playlist_pending.lock().get_mut(&chat_id) {
                        p.songs.retain(|x| x.Id != song.Id);
                    }
                    if let Err(e) = emby.refresh_library().await {
                        warn!(">> EMBY: refresh after delete failed: {}", e);
                    }
                    info!(">> PLAYLIST: deleted item {} ({}) from library", song.Name, song.Id);
                    let text = format!("🗑 已从音乐库删除：{}（歌单会自动移除该歌）", song.Name);
                    let kb = InlineKeyboardMarkup::new(vec![vec![
                        InlineKeyboardButton::callback("🔙 返回歌单歌曲列表", "playlist:songback"),
                    ]]);
                    edit_message_with_kb(&bot, chat_id, msg_id, text, kb).await;
                }
                Err(e) => {
                    warn!(">> PLAYLIST: delete item failed: {}", e);
                    edit_message(&bot, chat_id, msg_id, format!("❌ 删除失败：{}", e)).await;
                }
            }
        }
        Some(&"songback") => {
            // 返回歌单歌曲列表（重新拉取渲染）
            let Some(playlist) = ctx.playlist.lock().clone() else {
                edit_message(&bot, chat_id, msg_id, "❌ 未设置歌单，先用 /playlist 选择或创建").await;
                return Ok(());
            };
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

/// 歌单列表展示模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlaylistListMode {
    /// /playlist 普通选择（点序号打开歌单），底部带 🗑 删除入口
    Open,
    /// 入库后 ➕ 加入歌单（点序号加入该歌单），底部带 🔙 返回
    AddTo,
    /// 删除歌单模式（点序号进入两步确认），底部带 🔙 返回
    Delete,
}

/// 拉取并渲染 Emby 歌单列表（序号按钮 + 底部操作行）。
/// msg_id 为 None 时发新消息（/playlist 命令），否则编辑现有消息。
pub(crate) async fn render_playlist_list_ui(
    bot: &Bot,
    ctx: &Context,
    chat_id: ChatId,
    msg_id: Option<teloxide::types::MessageId>,
    mode: PlaylistListMode,
) {
    let Some(emby) = ctx.emby.clone() else {
        let text = "Emby 联动未启用";
        match msg_id {
            Some(id) => edit_message(bot, chat_id, id, text).await,
            None => {
                let _ = bot.send_message(chat_id, text).await;
            }
        }
        return;
    };
    let lists = match emby.list_playlists().await {
        Ok(l) => l,
        Err(e) => {
            let text = format!("❌ 读取歌单列表失败：{}", e);
            match msg_id {
                Some(id) => edit_message(bot, chat_id, id, text).await,
                None => {
                    let _ = bot.send_message(chat_id, text).await;
                }
            }
            return;
        }
    };
    if lists.is_empty() {
        let text = "📋 还没有 Emby 歌单。用 /playlist <歌单名> 创建";
        match msg_id {
            Some(id) => edit_message(bot, chat_id, id, text).await,
            None => {
                let _ = bot.send_message(chat_id, text).await;
            }
        }
        return;
    }
    let cur = ctx.playlist.lock().clone();
    let (mut text, cb_prefix) = match mode {
        PlaylistListMode::Open => (
            format!("📋 选择歌单（点下方序号打开，共 {} 个）：\n", lists.len()),
            "playlist:open:",
        ),
        PlaylistListMode::AddTo => (
            format!("➕ 选择要加入的歌单（点序号加入，共 {} 个）：\n", lists.len()),
            "playlist:addpick:",
        ),
        PlaylistListMode::Delete => (
            format!("🗑 选择要删除的歌单（共 {} 个，点序号两步确认）：\n", lists.len()),
            "playlist:delpick:",
        ),
    };
    for (i, p) in lists.iter().enumerate() {
        let mark = if cur.as_ref().is_some_and(|c| c.id == p.id) {
            " ✅当前"
        } else {
            ""
        };
        text.push_str(&format!("{}. {}{}\n", i + 1, p.name, mark));
    }
    ctx.playlist_list_pending.lock().insert(
        chat_id,
        PendingPlaylistList {
            lists: lists.clone(),
            created: Instant::now(),
        },
    );
    let buttons: Vec<InlineKeyboardButton> = lists
        .iter()
        .enumerate()
        .map(|(i, _)| {
            InlineKeyboardButton::callback((i + 1).to_string(), format!("{}{}", cb_prefix, i))
        })
        .collect();
    let mut rows: Vec<Vec<InlineKeyboardButton>> =
        buttons.chunks(8).map(|c| c.to_vec()).collect();
    let bottom = match mode {
        PlaylistListMode::Open => {
            vec![InlineKeyboardButton::callback("🗑 删除歌单", "playlist:dellist")]
        }
        PlaylistListMode::AddTo => {
            vec![InlineKeyboardButton::callback("🔙 返回", "playlist:actback")]
        }
        PlaylistListMode::Delete => {
            vec![InlineKeyboardButton::callback("🔙 返回", "playlist:list")]
        }
    };
    rows.push(bottom);
    let kb = InlineKeyboardMarkup::new(rows);
    match msg_id {
        Some(id) => edit_message_with_kb(bot, chat_id, id, text, kb).await,
        None => {
            let mut req = bot.send_message(chat_id, text);
            req.payload_mut().reply_markup =
                Some(teloxide::types::ReplyMarkup::InlineKeyboard(kb));
            if let Err(e) = req.await {
                warn!(">> PLAYLIST: send list failed: {}", e);
            }
        }
    }
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
            keyword: String::new(),
        },
    );
    let mut text = format!(
        "📋 歌单「{}」（{} 首），点序号选择播放/删除（60 秒内有效）：\n",
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
                format!("playlist:act:{}", i),
            )
        })
        .collect();
    let mut rows: Vec<Vec<InlineKeyboardButton>> =
        buttons.chunks(8).map(|c| c.to_vec()).collect();
    // 最后一行：返回歌单列表
    rows.push(vec![InlineKeyboardButton::callback("🔙 返回歌单列表", "playlist:list")]);
    edit_message_with_kb(bot, chat_id, msg_id, text, InlineKeyboardMarkup::new(rows)).await;
}

/// 编辑消息文本（去掉键盘）。
/// 信息卡是媒体消息（photo/audio）时 editMessageText 会 400，回退编辑 caption。
async fn edit_message(
    bot: &Bot,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    text: impl Into<String>,
) {
    let text = text.into();
    let mut req = bot.edit_message_text(chat_id, msg_id, text.clone());
    req.payload_mut().reply_markup = None;
    if req.await.is_ok() {
        return;
    }
    // 媒体消息（photo 信息卡）没有 text：编辑 caption
    let mut req = bot.edit_message_caption(chat_id, msg_id);
    req.payload_mut().caption = Some(text);
    req.await.ok();
}

/// 编辑消息文本 + 内联键盘。媒体消息回退编辑 caption + 键盘。
async fn edit_message_with_kb(
    bot: &Bot,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    text: impl Into<String>,
    kb: InlineKeyboardMarkup,
) {
    let text = text.into();
    let mut req = bot.edit_message_text(chat_id, msg_id, text.clone());
    req.payload_mut().reply_markup = Some(kb.clone());
    if req.await.is_ok() {
        return;
    }
    // 媒体消息回退：编辑 caption + 键盘
    let mut req = bot.edit_message_caption(chat_id, msg_id);
    req.payload_mut().caption = Some(text);
    req.payload_mut().reply_markup = Some(kb.clone());
    if req.await.is_ok() {
        return;
    }
    // 极端情况（caption 也不可编辑）至少更新键盘
    let mut req = bot.edit_message_reply_markup(chat_id, msg_id);
    req.payload_mut().reply_markup = Some(kb);
    req.await.ok();
}
