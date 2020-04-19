use std::{
  env,
  sync::{Arc, RwLock},
};

use serenity::{
  framework::StandardFramework,
  model::{channel::Channel, gateway::Ready},
  prelude::*,
  utils,
};

mod audio;
mod common;
mod search;
use common::*;

struct Handler;
impl EventHandler for Handler {
  fn ready(&self, _: Context, ready: Ready) {
    log::info!("{} connected", ready.user.name);
  }
}

const SAVED_VOLUME_MAP_FILE_NAME: &str = "volume_map.json";

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
  dotenv::dotenv().ok();
  if env::var("RUST_LOG").is_err() {
    env::set_var("RUST_LOG", "kitt=debug,info");
  }
  pretty_env_logger::init();
  let token = env::var("DISCORD_TOKEN")?;
  let youtube_api_key = env::var("YOUTUBE_API_KEY")?;
  let soundcloud_client_id = env::var("SOUNDCLOUD_CLIENT_ID")?;
  let search_client = reqwest::blocking::Client::new();
  let mut client = Client::new(&token, Handler).expect("Err creating client");

  let thread_pool = threadpool::Builder::new()
    .thread_name(String::from("audio-runtime"))
    .thread_stack_size(8_000_000)
    .build();

  let saved_volume_map: std::collections::HashMap<u64, f32> =
    match std::fs::File::open(SAVED_VOLUME_MAP_FILE_NAME) {
      Ok(file) => serde_json::from_reader(std::io::BufReader::new(file)).ok(),
      Err(_) => None,
    }
    .unwrap_or_default();

  {
    let mut data = client.data.write();
    data.insert::<audio::ThreadPoolKey>(Arc::new(Mutex::new(thread_pool)));
    data.insert::<audio::QueueKey>(Arc::new(RwLock::new(std::collections::HashMap::new())));
    data.insert::<audio::VoiceManager>(Arc::clone(&client.voice_manager));
    data.insert::<audio::YouTubeApiKey>(Arc::new(youtube_api_key));
    data.insert::<audio::SoundCloudApiKey>(Arc::new(soundcloud_client_id));
    data.insert::<audio::SearchClientKey>(Arc::new(search_client));
    data.insert::<audio::VolumeMapKey>(Arc::new(saved_volume_map));
  }

  client.with_framework(
    StandardFramework::new()
      .configure(|c| c.with_whitespace(true).prefix("."))
      .after(|ctx, msg, command_name, error| match error {
        Ok(()) => log::info!("Successfully executed {}", command_name),
        Err(why) => {
          log::error!("Failed to execute {}: {:?}", command_name, why);
          match msg.channel(&ctx.cache) {
            Some(Channel::Guild(guild_chan)) => {
              if let Err(why) = send_message(
                ctx,
                &guild_chan.read(),
                MessageParams {
                  title: Some(String::from("Error")),
                  message: Some(why.0),
                  color: Some(utils::Color::RED.tuple()),
                  ..MessageParams::default()
                },
              ) {
                log::error!("Error sending failure message: {:?}", why);
              }
            }
            _ => log::warn!(
              "Attempted to send failure message to non guild channel: {:?}",
              why
            ),
          }
        }
      })
      .group(&audio::AUDIO_GROUP),
  );

  let ctrlc_shard_manager = Arc::clone(&client.shard_manager);
  let ctrlc_data = Arc::clone(&client.data);

  ctrlc::set_handler(move || {
    // save guild volumes
    if let Some(queue_map) = ctrlc_data.read().get::<audio::QueueKey>() {
      if let Ok(queue_map) = queue_map.read() {
        let volume_map = queue_map.iter().fold(
          std::collections::HashMap::new(),
          |mut acc, (guild, queue)| {
            if let Ok(queue) = queue.read() {
              acc.insert(guild, queue.volume.clone());
            }
            acc
          },
        );
        match serde_json::to_string_pretty(&volume_map) {
          Ok(json) => {
            if let Err(why) = std::fs::write(SAVED_VOLUME_MAP_FILE_NAME, json) {
              log::error!("Failed to save volume_map: {}", &why);
            }
          }
          Err(why) => log::error!("Failed to serialize volume map: {}", &why),
        }
      }
    }
    ctrlc_shard_manager.lock().shutdown_all();
  })?;

  client.start()?;
  Ok(())
}
