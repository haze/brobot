use serenity::{
  builder::{CreateMessage, EditMessage},
  model::{self, channel::Message, permissions::Permissions},
  prelude::*,
};

pub type RGB = (u8, u8, u8);

#[derive(Default)]
pub struct MessageParams {
  pub fields: Option<Vec<(String, String, bool)>>,
  pub message: Option<String>,
  pub title: Option<String>,
  pub color: Option<RGB>,
}

/// 1. Check if we can send an embed in this channel, if so:
///     1. Create embed
pub fn simple_message(builder: &mut CreateMessage, params: MessageParams, perms: Permissions) {
  if perms.send_messages() && perms.embed_links() {
    builder.embed(|embed| {
      if let Some(title) = params.title {
        embed.title(title);
      }
      if let Some(message) = params.message {
        embed.description(message);
      }
      if let Some(color) = params.color {
        embed.color(color);
      }
      if let Some(fields) = params.fields {
        embed.fields(fields.into_iter());
      }
      embed
    });
  } else {
    unimplemented!()
  }
}

pub fn simple_message_edit(builder: &mut EditMessage, params: MessageParams, perms: Permissions) {
  if perms.send_messages() && perms.embed_links() {
    builder.embed(|embed| {
      if let Some(title) = params.title {
        embed.title(title);
      }
      if let Some(message) = params.message {
        embed.description(message);
      }
      if let Some(color) = params.color {
        embed.color(color);
      }
      if let Some(fields) = params.fields {
        embed.fields(fields.into_iter());
      }
      embed
    });
  } else {
    unimplemented!()
  }
}

pub async fn send_message(
  ctx: &Context,
  channel: &model::channel::GuildChannel,
  params: MessageParams,
) -> Result<Message, SerenityError> {
  let bot_id = ctx.http.get_current_user().await?.id;
  let perms = channel.permissions_for_user(&ctx.cache, bot_id).await?;

  channel
    .send_message(&ctx.http, |m| {
      simple_message(m, params, perms);
      m
    })
    .await
    .map_err(|e| {
      log::error!("Failed to send message: {:?}", &e);
      e
    })
}
