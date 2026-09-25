use super::{
    callback::render_favs_results,
    callback::render_library_results,
    callback::render_music_results,
    callback::render_playlist_list_ui,
    callback::PlaylistListMode,
    MyDialogue,
};
use crate::{
    context::Context,
    emby::{PendingEmby, PlaylistCtx},
    storage::MyStorage,
    utils::gen_key,
};
use anyhow::Result;
use std::time::Instant;
use teloxide::{
    Bot,
    dispatching::UpdateHandler,
    dptree::case,
    macros::BotCommands,
    prelude::Requester as _,
    requests::HasPayload as _,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, MediaKind, MediaText, Message, MessageCommon, MessageKind},
    utils::command::BotCommands as _,
};
use tracing::{info, warn};

#[derive(BotCommands, Clone)]
#[command(
    rename_rule = "lowercase",
    description = "These commands are supported:"
)]
enum Command {
    #[command(description = "This is a bot to sync files from chat.")]
    Start,
    #[command(description = "Display this text.")]
    Help,
    #[command(description = "Show the current state.")]
    State,
    #[command(description = "Switch among paused, active, partially-active state.")]
    Toggle,
    #[command(description = "Print current bypass key in the server side.")]
    BypassKey,
    #[command(description = "Clear all downloaded files in normal directory.")]
    Clear,
    #[command(description = "音乐库管理器：搜库内歌曲，未命中可在线找歌入库，e.g. /music 晴天")]
    Music(String),
    #[command(description = "已并入 /music，请用 /music 搜索（兼容保留）")]
    Emby(String),
    #[command(description = "Create/open an Emby playlist, e.g. /playlist 我的歌单")]
    Playlist(String),
    #[command(rename = "favs", description = "List your favorited music")]
    Favs,
}

pub fn cmd_handler() -> UpdateHandler<anyhow::Error> {
    teloxide::filter_command::<Command, _>()
        .branch(
            case![Command::Start].endpoint(async |bot: Bot, msg: Message| {
                bot.send_message(
                    msg.chat.id,
                    "This is a bot to sync files from chat. Enter /help to see all commands.",
                )
                .await?;
                Ok(())
            }),
        )
        .branch(
            case![Command::Help].endpoint(async |bot: Bot, msg: Message| {
                bot.send_message(msg.chat.id, Command::descriptions().to_string())
                    .await?;
                Ok(())
            }),
        )
        .branch(case![Command::BypassKey].endpoint(
            async |bot: Bot, dialogue: MyDialogue, msg: Message, ctx: Context| {
                if !auth(&bot, &dialogue, &msg, &ctx).await? {
                    info!(">> BOT: auth not pass");
                    return Ok(());
                }
                info!(">> BOT: BypassKey: /toggle {}", ctx.bypasskey.read());
                Ok(())
            },
        ))
        .branch(
            case![Command::State].endpoint(async |bot: Bot, msg: Message, db: MyStorage| {
                let state = db.get_chat_state(msg.chat.id).await?;
                bot.send_message(msg.chat.id, format!("Current State: {}", state))
                    .await?;
                Ok(())
            }),
        )
        .branch(case![Command::Toggle].endpoint(
            async |bot: Bot, dialogue: MyDialogue, msg: Message, ctx: Context, db: MyStorage| {
                if !auth(&bot, &dialogue, &msg, &ctx).await? {
                    info!(">> BOT: auth not pass");
                    return Ok(());
                }
                let state = db.toggle_chat_state(msg.chat.id).await?;
                info!(">> BOT: curren state of {} {}", msg.chat.id, state);
                bot.send_message(msg.chat.id, format!("Current State: {}", state))
                    .await?;
                Ok(())
            },
        ))
        .branch(case![Command::Clear].endpoint(
            async |bot: Bot, dialogue: MyDialogue, msg: Message, ctx: Context, db: MyStorage| {
                if !auth(&bot, &dialogue, &msg, &ctx).await? {
                    info!(">> BOT: auth not pass");
                    return Ok(());
                }
                let (count, bytes) = db.summarize_normal().await?;
                if count == 0 {
                    bot.send_message(msg.chat.id, "normal 目录没有文件可清空")
                        .await?;
                    return Ok(());
                }
                let text = format!(
                    "📁 normal 目录共 {} 个文件（约 {}）\n确认清空？此操作不可恢复",
                    count,
                    crate::utils::format_size(bytes)
                );
                let keyboard = InlineKeyboardMarkup::new(vec![vec![
                    InlineKeyboardButton::callback("✅ 确认清空", "clear:yes"),
                    InlineKeyboardButton::callback("❌ 取消", "clear:no"),
                ]]);
                let mut req = bot.send_message(msg.chat.id, text);
                req.payload_mut().reply_markup = Some(teloxide::types::ReplyMarkup::InlineKeyboard(keyboard));
                req.await?;
                Ok(())
            },
        ))
        .branch(case![Command::Music(keyword)].endpoint(
            async |bot: Bot, msg: Message, ctx: Context, keyword: String| {
                let keyword = keyword.trim().to_string();
                if keyword.is_empty() {
                    bot.send_message(msg.chat.id, "用法：/music <歌名> [歌手]，例如 /music 晴天 周杰伦\n可加音质/来源前缀：/music flac 晴天、/music qq 晴天")
                        .await?;
                    return Ok(());
                }
                // 解析可选前缀：音质（flac/ape/wav/m4a/320/128）或来源（kw/qq/mg/...）
                let (pref, plug, keyword) = parse_music_args(&keyword);
                // ① 先搜 Emby 音乐库（音乐库管理器：库内优先，命中直接管理）
                if let Some(emby) = &ctx.emby {
                    match emby.search_songs(&keyword, 8).await {
                        Ok(songs) if !songs.is_empty() => {
                            render_library_results(&bot, &ctx, msg.chat.id, None, &keyword, &songs)
                                .await;
                            return Ok(());
                        }
                        Ok(_) => {}
                        Err(e) => {
                            warn!(">> EMBY: search failed: {}, fallback to online", e);
                        }
                    }
                }
                // ② 库内未命中：在线找歌（sqmusic）
                let Some(sqm) = &ctx.sqmusic else {
                    bot.send_message(
                        msg.chat.id,
                        "音乐库没有「{}」，且 sqmusic 联动未启用（未配置 SQMUSIC_URL）",
                    )
                    .await?;
                    return Ok(());
                };
                let plug_name = plug.as_deref().unwrap_or("kw");
                // search with kw first (most songs free to download)
                let songs = match sqm.search(plug_name, &keyword, 5).await {
                    Ok(s) if !s.is_empty() => s,
                    Ok(_) => {
                        bot.send_message(msg.chat.id, format!("在线也未找到「{}」相关歌曲", keyword))
                            .await?;
                        return Ok(());
                    }
                    Err(e) => {
                        warn!(">> SQMUSIC: search failed: {}", e);
                        bot.send_message(msg.chat.id, format!("搜索失败：{}", e)).await?;
                        return Ok(());
                    }
                };
                let top = songs.into_iter().take(5).collect::<Vec<_>>();
                render_music_results(&bot, &ctx, msg.chat.id, None, &keyword, &top, pref.as_deref())
                    .await;
                Ok(())
            },
        ))
        .branch(case![Command::Emby(keyword)].endpoint(
            async |bot: Bot, msg: Message, ctx: Context, keyword: String| {
                let keyword = keyword.trim().to_string();
                if keyword.is_empty() {
                    bot.send_message(
                        msg.chat.id,
                        "ℹ️ /emby 已并入 /music：请用 /music <歌名> 搜索音乐库，未命中可在线找歌入库",
                    )
                    .await?;
                    return Ok(());
                }
                // 兼容旧习惯：/emby <关键词> = /music 库内搜索（同一套交互）
                let Some(emby) = &ctx.emby else {
                    bot.send_message(
                        msg.chat.id,
                        "Emby 联动未启用（服务端未配置 EMBY_URL/EMBY_API_KEY）",
                    )
                    .await?;
                    return Ok(());
                };
                let songs = match emby.search_songs(&keyword, 8).await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(">> EMBY: search failed: {}", e);
                        bot.send_message(msg.chat.id, format!("Emby 查询失败：{}", e)).await?;
                        return Ok(());
                    }
                };
                if songs.is_empty() {
                    bot.send_message(msg.chat.id, format!("Emby 音乐库没有「{}」相关歌曲", keyword))
                        .await?;
                    return Ok(());
                }
                render_library_results(&bot, &ctx, msg.chat.id, None, &keyword, &songs).await;
                Ok(())
            },
        ))
        .branch(case![Command::Playlist(name)].endpoint(
            async |bot: Bot, msg: Message, ctx: Context, name: String| {
                let Some(emby) = &ctx.emby else {
                    bot.send_message(msg.chat.id, "Emby 联动未启用（未配置 EMBY_URL/EMBY_API_KEY）")
                        .await?;
                    return Ok(());
                };
                let name = name.trim();
                if name.is_empty() {
                    // 无参数：直接列出 Emby 全部歌单（序号按钮 + 🗑 删除入口）
                    render_playlist_list_ui(&bot, &ctx, msg.chat.id, None, PlaylistListMode::Open).await;
                    return Ok(());
                }
                match emby.find_or_create_playlist(&name).await {
                    Ok(id) => {
                        ctx.playlist
                            .lock()
                            .replace(PlaylistCtx { id: id.clone(), name: name.to_string() });
                        info!(">> EMBY: playlist ready {} ({})", name, id);
                        // 就绪后直接列出歌单内歌曲
                        let songs = match emby.playlist_items(&id).await {
                            Ok(s) => s,
                            Err(e) => {
                                bot.send_message(
                                    msg.chat.id,
                                    format!(
                                        "✅ 歌单「{}」已就绪（Emby Id {}）。\n但读取歌曲列表失败：{}",
                                        name, id, e
                                    ),
                                )
                                .await?;
                                return Ok(());
                            }
                        };
                        if songs.is_empty() {
                            bot.send_message(
                                msg.chat.id,
                                format!(
                                    "✅ 歌单「{}」已就绪（Emby Id {}）。\n📋 歌单还是空的：用 /music 下载试听后 📥 入库，再点 ➕ 加入歌单",
                                    name, id
                                ),
                            )
                            .await?;
                        } else {
                            ctx.playlist_pending.lock().insert(
                                msg.chat.id,
                                PendingEmby {
                                    songs: songs.clone(),
                                    created: Instant::now(),
                                    keyword: String::new(),
                                },
                            );
                            let mut text = format!(
                                "✅ 歌单「{}」已就绪（Emby Id {}），共 {} 首，点序号选择播放/删除（60 秒内有效）：\n",
                                name,
                                id,
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
                                    InlineKeyboardButton::callback(
                                        (i + 1).to_string(),
                                        format!("playlist:act:{}", i),
                                    )
                                })
                                .collect();
                            let rows: Vec<Vec<InlineKeyboardButton>> =
                                buttons.chunks(8).map(|c| c.to_vec()).collect();
                            let mut req = bot.send_message(msg.chat.id, text);
                            req.payload_mut().reply_markup =
                                Some(teloxide::types::ReplyMarkup::InlineKeyboard(
                                    InlineKeyboardMarkup::new(rows),
                                ));
                            req.await?;
                        }
                    }
                    Err(e) => {
                        warn!(">> EMBY: playlist failed: {}", e);
                        bot.send_message(msg.chat.id, format!("❌ 歌单操作失败：{}", e)).await?;
                    }
                }
                Ok(())
            },
        ))
        .branch(case![Command::Favs].endpoint(
            async |bot: Bot, msg: Message, db: MyStorage| {
                render_favs_results(&bot, msg.chat.id, None, &db).await;
                Ok(())
            },
        ))
}

/// 解析 /music 参数中的可选前缀：音质（flac/320 等）与来源（kw/qq 等），返回 (音质, 来源, 歌名词)。
fn parse_music_args(input: &str) -> (Option<String>, Option<String>, String) {
    let mut pref: Option<String> = None;
    let mut plug: Option<String> = None;
    let mut rest: Vec<&str> = Vec::new();
    for p in input.split_whitespace() {
        let low = p.to_ascii_lowercase();
        if pref.is_none()
            && matches!(low.as_str(), "flac" | "ape" | "wav" | "m4a" | "ogg" | "320" | "128")
        {
            pref = Some(low);
        } else if plug.is_none()
            && matches!(low.as_str(), "kw" | "qq" | "mg" | "mgg" | "netease" | "kg" | "apple" | "qqvip")
        {
            plug = Some(low);
        } else {
            rest.push(p);
        }
    }
    (pref, plug, rest.join(" "))
}

async fn auth(bot: &Bot, dialogue: &MyDialogue, msg: &Message, ctx: &Context) -> Result<bool> {
    if msg.from.as_ref().map(|user| {
        ctx.bypass_users
            .as_ref()
            .is_some_and(|bypass_users| bypass_users.contains(&user.id))
    }) != Some(true)
    {
        // check bypass_pwd
        match msg {
            Message {
                kind:
                    MessageKind::Common(MessageCommon {
                        media_kind: MediaKind::Text(MediaText { text, .. }),
                        ..
                    }),
                ..
            } if matches!(text.split_once(" "), Some((_, key)) if key == *ctx.bypasskey.read()) => {
                // renew bypass_pwd
                let new = gen_key();
                info!(">> BOT: New bypasskey: {}", new);
                crate::utils::save_key(&ctx.data_dir, &new);
                *ctx.bypasskey.write() = new;
                Ok(true)
            }
            _ => {
                bot.send_message(
                    msg.chat.id,
                    "Permission denied: You are not in allow users list or invalid password",
                )
                .await?;
                dialogue.exit().await?;
                Ok(false)
            }
        }
    } else {
        Ok(true)
    }
}
