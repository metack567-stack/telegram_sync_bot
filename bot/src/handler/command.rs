use super::MyDialogue;
use crate::{
    context::Context,
    sqm::PendingMusic,
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
    #[command(description = "Search and download music via sqmusic, e.g. /music 晴天")]
    Music(String),
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
                let Some(sqm) = &ctx.sqmusic else {
                    bot.send_message(
                        msg.chat.id,
                        "sqmusic 联动未启用（服务端未配置 SQMUSIC_URL）",
                    )
                    .await?;
                    return Ok(());
                };
                let keyword = keyword.trim();
                if keyword.is_empty() {
                    bot.send_message(msg.chat.id, "用法：/music <歌名> [歌手]，例如 /music 晴天 周杰伦")
                        .await?;
                    return Ok(());
                }
                // search with kw first (most songs free to download)
                let songs = match sqm.search("kw", keyword, 5).await {
                    Ok(s) if !s.is_empty() => s,
                    Ok(_) => {
                        bot.send_message(msg.chat.id, format!("未找到「{}」相关歌曲", keyword))
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
                ctx.music_pending.lock().insert(
                    msg.chat.id,
                    PendingMusic {
                        songs: top.clone(),
                        created: Instant::now(),
                    },
                );
                let mut text = format!(
                    "🎵 搜索到「{}」相关歌曲，回复数字选择（60 秒内有效）：\n",
                    keyword
                );
                for (i, s) in top.iter().enumerate() {
                    let artist = if s.artistName.is_empty() {
                        "未知歌手".to_string()
                    } else {
                        s.artistName.join("/")
                    };
                    let album = s.albumName.clone().unwrap_or_else(|| "未知专辑".to_string());
                    let br = crate::sqm::pick_br_type(&s.brTypes)
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
                bot.send_message(msg.chat.id, text).await?;
                Ok(())
            },
        ))
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
