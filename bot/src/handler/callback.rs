use crate::{
    context::Context,
    storage::MyStorage,
};
use anyhow::Result;
use teloxide::{
    Bot,
    dispatching::{UpdateFilterExt as _, UpdateHandler},
    prelude::Requester as _,
    requests::HasPayload as _,
    types::{CallbackQuery, ChatId, Update},
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
    if !data.starts_with("clear:") && !data.starts_with("music:dl:") {
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

/// 音质选择回调：music:dl:<song_idx>:<brType>，brType 为 "auto" 表示按默认策略自动选。
async fn handle_music(
    bot: Bot,
    ctx: Context,
    chat_id: ChatId,
    msg_id: teloxide::types::MessageId,
    data: String,
) -> Result<()> {
    let parts: Vec<&str> = data.split(':').collect();
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
        let bot = bot.clone();
        tokio::spawn(async move {
            if let Err(e) = super::message::download_and_send(bot, sqm, music_dir, chat_id, song, br).await {
                warn!(">> SQMUSIC: download flow failed: {}", e);
            }
        });
    }
    Ok(())
}
