use crate::{context::Context, storage::MyStorage};
use anyhow::Result;
use teloxide::{
    Bot,
    dispatching::{UpdateFilterExt as _, UpdateHandler},
    prelude::Requester as _,
    requests::HasPayload as _,
    types::{CallbackQuery, Update},
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
    if !data.starts_with("clear:") {
        return Ok(());
    }
    // owner-only: anyone who can see the message could tap the button otherwise
    if !ctx
        .bypass_users
        .as_ref()
        .is_some_and(|users| users.contains(&q.from.id))
    {
        info!(">> BOT: clear callback from unauthorized user {}", q.from.id);
        return Ok(());
    }
    let Some(chat_id) = q.message.as_ref().map(|m| m.chat().id) else {
        return Ok(());
    };
    let Some(msg_id) = q.message.as_ref().map(|m| m.id()) else {
        return Ok(());
    };
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
