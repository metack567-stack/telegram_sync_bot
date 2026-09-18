use super::MyDialogue;
use crate::{context::Context, storage::MyStorage, utils::gen_key};
use anyhow::Result;
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
use tracing::info;

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
