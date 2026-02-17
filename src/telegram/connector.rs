use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::FileId;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::providers::UserContent;

use super::approval::parse_callback_data;
use super::session::{InboundMessage, SessionCommand};

/// Handle an incoming Telegram message. Extracts text and photos,
/// routes to the session manager.
pub async fn handle_message(
    bot: Bot,
    msg: Message,
    session_tx: mpsc::Sender<SessionCommand>,
    allowed_chats: Option<Vec<i64>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let chat_id = msg.chat.id;

    // Chat ID allowlisting: unauthorized chats receive no response (opacity).
    if let Some(ref allowed) = allowed_chats
        && !allowed.contains(&chat_id.0)
    {
        info!(chat_id = %chat_id, "unauthorized chat, ignoring");
        return Ok(());
    }

    let mut content = Vec::new();

    // Handle photos: download largest size and base64-encode.
    if let Some(photos) = msg.photo()
        && let Some(largest) = photos.last()
    {
        match download_photo(&bot, largest.file.id.clone()).await {
            Ok(data) => {
                content.push(UserContent::Image {
                    media_type: "image/jpeg".to_owned(),
                    data,
                });
            }
            Err(e) => {
                warn!(chat_id = %chat_id, error = %e, "failed to download photo");
            }
        }
    }

    // Handle text content (including captions on photos).
    if let Some(text) = msg.text().or(msg.caption())
        && !text.is_empty()
    {
        content.push(UserContent::Text(text.to_owned()));
    }

    // Handle documents: forward as text description (full processing deferred).
    if let Some(doc) = msg.document() {
        let desc = match &doc.file_name {
            Some(name) => format!("[Document: {name}]"),
            None => "[Document attached]".to_owned(),
        };
        content.push(UserContent::Text(desc));
    }

    if content.is_empty() {
        return Ok(());
    }

    let _ = session_tx
        .send(SessionCommand::Message {
            chat_id,
            message: InboundMessage { content },
        })
        .await;

    Ok(())
}

/// Handle a callback query (inline keyboard button press).
pub async fn handle_callback(
    bot: Bot,
    query: CallbackQuery,
    session_tx: mpsc::Sender<SessionCommand>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(data) = &query.data
        && let Some((id, approved)) = parse_callback_data(data)
    {
        let _ = session_tx
            .send(SessionCommand::ApprovalCallback { id, approved })
            .await;

        // Acknowledge the callback to remove the loading indicator.
        let answer_text = if approved { "Approved" } else { "Denied" };
        let _ = bot
            .answer_callback_query(query.id.clone())
            .text(answer_text)
            .await;
    }

    Ok(())
}

/// Download a photo from Telegram and return it as base64-encoded data.
async fn download_photo(
    bot: &Bot,
    file_id: FileId,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let file = bot.get_file(file_id).await?;
    let mut buf = Vec::new();
    bot.download_file(&file.path, &mut buf).await?;
    Ok(BASE64.encode(&buf))
}
