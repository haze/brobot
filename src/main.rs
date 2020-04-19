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
use common::*;

struct Handler;
impl EventHandler for Handler {
  fn ready(&self, _: Context, ready: Ready) {
    log::info!("{} connected", ready.user.name);
  }
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
  dotenv::dotenv().ok();
  if env::var("RUST_LOG").is_err() {
    env::set_var("RUST_LOG", "kitt=debug,info");
  }
  pretty_env_logger::init();
  let token = env::var("DISCORD_TOKEN")?;
  let mut client = Client::new(&token, Handler).expect("Err creating client");

  let thread_pool = threadpool::Builder::new()
    .thread_name(String::from("audio-runtime"))
    .thread_stack_size(8_000_000)
    .build();

  {
    let mut data = client.data.write();
    data.insert::<audio::ThreadPoolKey>(Arc::new(Mutex::new(thread_pool)));
    data.insert::<audio::QueueKey>(Arc::new(RwLock::new(std::collections::HashMap::new())));
    data.insert::<audio::VoiceManager>(Arc::clone(&client.voice_manager));
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

  ctrlc::set_handler(move || {
    ctrlc_shard_manager.lock().shutdown_all();
  })?;

  client.start()?;
  Ok(())
}
