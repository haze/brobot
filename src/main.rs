use std::{env, sync::Arc};

use serenity::{
  framework::{
    standard::{macros::hook, CommandError},
    StandardFramework,
  },
  http::Http,
  model::{
    channel::{Channel, Message},
    gateway::Ready,
    id::UserId,
  },
  prelude::*,
  utils,
};

mod audio;
mod common;
mod error;
mod search;
use common::*;

struct Handler;

#[serenity::async_trait]
impl EventHandler for Handler {
  async fn ready(&self, _: Context, ready: Ready) {
    log::info!("{} connected", ready.user.name);
  }
}

const SAVED_VOLUME_MAP_FILE_NAME: &str = "volume_map.json";

#[hook]
async fn after_hook(
  ctx: &Context,
  msg: &Message,
  command_name: &str,
  error: Result<(), CommandError>,
) {
  match error {
    Ok(()) => log::info!("Successfully executed {}", command_name),
    Err(why) => {
      log::error!("Failed to execute {}: {:?}", command_name, why);
      match msg.channel(&ctx.cache).await {
        Some(Channel::Guild(guild_chan)) => {
          if let Err(why) = send_message(
            ctx,
            &guild_chan,
            MessageParams {
              title: Some(String::from("Error")),
              message: Some(why.to_string()),
              color: Some(utils::Color::RED.tuple()),
              ..MessageParams::default()
            },
          )
          .await
          {
            log::error!("Error sending failure message: {:?}", why);
          }
        }
        _ => log::warn!(
          "Attempted to send failure message to non guild channel: {:?}",
          why
        ),
      }
    }
  }
}

pub struct BotIdKey;
impl TypeMapKey for BotIdKey {
  type Value = UserId;
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
  dotenv::dotenv().ok();
  if env::var("RUST_LOG").is_err() {
    env::set_var("RUST_LOG", "brobot=debug");
  }
  pretty_env_logger::init();
  let token = env::var("DISCORD_TOKEN")?;
  let youtube_api_key = env::var("YOUTUBE_API_KEY")?;
  let soundcloud_client_id = env::var("SOUNDCLOUD_CLIENT_ID")?;

  let search_client = reqwest::Client::new();
  let http = Http::new_with_token(&token);

  let bot_id = http.get_current_user().await.map(|info| info.id)?;

  let mut client = Client::new(&token)
    .event_handler(Handler)
    .framework(
      StandardFramework::new()
        .configure(|c| c.with_whitespace(true).prefix("."))
        .after(after_hook)
        .group(&audio::AUDIO_GROUP),
    )
    .await
    .expect("Err creating client");

  let saved_volume_map: std::collections::HashMap<u64, f32> =
    match std::fs::File::open(SAVED_VOLUME_MAP_FILE_NAME) {
      Ok(file) => serde_json::from_reader(std::io::BufReader::new(file)).ok(),
      Err(_) => None,
    }
    .unwrap_or_default();

  {
    let mut data = client.data.write().await;
    data.insert::<audio::QueueKey>(Arc::new(RwLock::new(std::collections::HashMap::new())));
    data.insert::<audio::VoiceManager>(Arc::clone(&client.voice_manager));
    data.insert::<audio::YouTubeApiKey>(Arc::new(youtube_api_key));
    data.insert::<audio::SoundCloudApiKey>(Arc::new(soundcloud_client_id));
    data.insert::<audio::SearchClientKey>(Arc::new(search_client));
    data.insert::<audio::VolumeMapKey>(Arc::new(saved_volume_map));
    data.insert::<BotIdKey>(bot_id);
  }

  let data_clone = Arc::clone(&client.data);
  let shard_manager_clone = Arc::clone(&client.shard_manager);
  let client_task = tokio::spawn(async move { client.start().await });

  tokio::signal::ctrl_c()
    .await
    .expect("Failed to read ctrl-c signal");

  shard_manager_clone.lock().await.shutdown_all().await;
  client_task.await.ok();

  log::info!("Running cleanup phase");
  if let Some(queue_map) = data_clone.read().await.get::<audio::QueueKey>() {
    log::info!("Saving volume map");
    let queue_map_guard = queue_map.read().await;
    let mut volume_map = std::collections::HashMap::new();
    for (guild, queue) in queue_map_guard.iter() {
      volume_map.insert(guild, queue.read().await.volume.clone());
    }
    match serde_json::to_string_pretty(&volume_map) {
      Ok(json) => {
        if let Err(why) = std::fs::write(SAVED_VOLUME_MAP_FILE_NAME, json) {
          log::error!("Failed to save volume_map: {}", &why);
        }
      }
      Err(why) => log::error!("Failed to serialize volume map: {}", &why),
    }
  }

  Ok(())
}
