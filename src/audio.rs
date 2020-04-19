use crate::common::*;
use crossbeam::channel;
use serde::Deserialize;
use serenity::{
  cache::CacheRwLock,
  client::bridge::voice::ClientVoiceManager,
  framework::standard::{
    macros::{command, group},
    Args, CommandError, CommandResult,
  },
  http::client::Http,
  model::{
    channel::{Channel, GuildChannel, Message},
    id::{GuildId, UserId},
  },
  prelude::{RwLock as SerenityRwLock, *},
  voice,
};
use std::{
  collections::HashMap,
  fmt,
  sync::{Arc, RwLock},
};
use threadpool::ThreadPool;

#[derive(Deserialize, Debug, Clone, Hash)]
struct EmbedInfo {
  #[serde(deserialize_with = "serde_aux::field_attributes::deserialize_string_from_number")]
  width: String,
  #[serde(deserialize_with = "serde_aux::field_attributes::deserialize_string_from_number")]
  height: String,
  author_name: String,
  author_url: String,

  #[serde(deserialize_with = "serde_aux::field_attributes::deserialize_string_from_number")]
  version: String,

  provider_url: String,
  provider_name: String,
  thumbnail_width: Option<usize>,
  thumbnail_height: Option<usize>,
  thumbnail_url: Option<String>,
  html: String,
  url: String,

  #[serde(rename = "type")]
  kind: String,
  title: String,
}

#[derive(Clone)]
struct ThreadSafeTrackStatus {
  pub state: Arc<RwLock<TrackState>>,
}

impl ThreadSafeTrackStatus {
  fn set_state(&self, state: TrackState) {
    if let Ok(mut inner_state) = self.state.write() {
      *inner_state = state
    }
  }

  fn get_url(&self) -> Option<String> {
    self.state.read().map(|track| track.url()).ok()
  }

  fn get_source(&self) -> Option<TrackSource> {
    self.state.read().map(|track| track.source()).ok()
  }

  fn ready(&self, path: std::path::PathBuf) {
    if let Some(source) = self.get_source() {
      self.set_state(TrackState::Ready { source, path })
    }
  }

  fn fail<E: 'static>(&self, error: Box<E>)
  where
    E: std::error::Error + Send + Sync,
  {
    if let Some(source) = self.get_source() {
      self.set_state(TrackState::Failed { source, error })
    }
  }

  // (Source, Url)
  fn get_info(&self) -> Option<(TrackSource, String)> {
    self
      .state
      .read()
      .map(|track| (track.source(), track.url()))
      .ok()
  }
}

#[derive(Copy, Clone, Debug)]
enum QueueWorkerEvent {
  SongFinished,
  SongFinishedDownloading,
  // skip, play, pause, etc
  StateUpdate,
}

struct NowPlaying {
  source: TrackSource,
  audio: voice::LockedAudio,
  worker_handle: std::thread::JoinHandle<()>,
}

type QueueWorkerSender = channel::Sender<QueueWorkerEvent>;
type QueueWorkerReceiver = channel::Receiver<QueueWorkerEvent>;

fn now_playing_task(queue_sender: QueueWorkerSender, audio: voice::LockedAudio) {
  loop {
    if audio.lock().finished {
      if let Err(why) = queue_sender.send(QueueWorkerEvent::SongFinished) {
        log::error!("Failed to send song finished notification: {}", &why);
      }
      break;
    }
  }
}

impl NowPlaying {
  fn new(
    source: TrackSource,
    audio: voice::LockedAudio,
    queue_sender: QueueWorkerSender,
  ) -> NowPlaying {
    // spawn task responsible for letting the queue know the song is done
    let worker_clone = Arc::clone(&audio);
    NowPlaying {
      source,
      audio,
      worker_handle: std::thread::spawn(move || now_playing_task(queue_sender, worker_clone)),
    }
  }
}

struct VoiceConnection {
  channel: Arc<SerenityRwLock<GuildChannel>>,
  handler: Arc<Mutex<voice::Handler>>,
}

pub struct Queue {
  currently_playing: Option<NowPlaying>,
  connected_channel: Option<VoiceConnection>,
  tracks: Vec<ThreadSafeTrackStatus>,
  runtime: Arc<Mutex<ThreadPool>>,
  worker_sender: QueueWorkerSender,
  last_queue_message: Option<LastQueueMessage>,
  bot_id: UserId,
  cache: CacheRwLock,
  http: Arc<Http>,
  paused: bool,
}

struct LastQueueMessage {
  sent_at: std::time::Instant,
  message: Message,
  channel: Arc<SerenityRwLock<GuildChannel>>,
}

// 1. if there is nothing playing, and a song just finished downloading, try to begin playing
// 2. if a song finished playing, try to play the next song
fn queue_worker_task(queue: Arc<RwLock<Queue>>, receiver: QueueWorkerReceiver) {
  fn next_song(q: &Arc<RwLock<Queue>>) {
    match q.write() {
      Ok(mut queue) => {
        if let Err(why) = queue.next_song() {
          log::error!("Can't play next song: {:?}", &why);
        }
        if let Err(why) = queue.worker_sender.send(QueueWorkerEvent::StateUpdate) {
          log::error!("Failed to send queue State Update event: {}", &why);
        }
      }
      Err(why) => {
        log::error!("Failed to acquire write lock for queue: {}", &why);
      }
    }
  }
  for event in receiver {
    let queue_read_ref = queue.read();
    if let Ok(queue_read) = &queue_read_ref {
      let is_playing_anything = queue_read.currently_playing.is_some();
      let can_play = queue_read.connected_channel.is_some() && !queue_read.paused;
      drop(queue_read_ref);
      {
        // every time we receive an event, update the last queue message (if any)
        let write_guard = queue.write();
        if let Ok(queue_write) = write_guard {
          let bot_id = queue_write.bot_id;
          if let Some(ref last_msg) = queue_write.last_queue_message {
            let mut msg = last_msg.message.clone();
            if let Ok(perms) = last_msg
              .channel
              .read()
              .permissions_for_user(&queue_write.cache, bot_id)
            {
              if let Err(why) = msg.edit(&queue_write.http, |mut cm| {
                crate::common::simple_message_edit(
                  &mut cm,
                  MessageParams {
                    message: Some(format!("{}", &queue_write)),
                    ..MessageParams::default()
                  },
                  perms,
                );
                cm
              }) {
                log::error!("Error updating last queue message: {}", &why);
              }
            } else {
              log::error!("Failed to determine permissions for last queue message update");
            }
          }
        }
      }
      match event {
        QueueWorkerEvent::SongFinished => {
          next_song(&queue);
        }
        QueueWorkerEvent::SongFinishedDownloading => {
          // play next song, if nothing is playing
          if !is_playing_anything && can_play {
            next_song(&queue);
          }
        }
        _ => {}
      }
    }
  }
}

impl Queue {
  pub fn new(
    runtime: Arc<Mutex<ThreadPool>>,
    bot_id: UserId,
    cache: CacheRwLock,
    http: Arc<Http>,
  ) -> Arc<RwLock<Queue>> {
    let (worker_sender, receiver) = channel::unbounded();
    let queue = Arc::new(RwLock::new(Queue {
      currently_playing: None,
      connected_channel: None,
      tracks: Vec::new(),
      worker_sender,
      runtime: Arc::clone(&runtime),
      last_queue_message: None,
      paused: false,
      cache,
      http,
      bot_id,
    }));
    let queue_clone = Arc::clone(&queue);
    runtime
      .lock()
      .execute(move || queue_worker_task(queue_clone, receiver));
    queue
  }
}

pub struct ThreadPoolKey;

impl TypeMapKey for ThreadPoolKey {
  type Value = Arc<Mutex<ThreadPool>>;
}

pub struct QueueKey;

type ThreadSafeQueue = Arc<RwLock<Queue>>;
type ThreadSafeGuildIdMap<T> = Arc<RwLock<HashMap<GuildId, T>>>;

impl TypeMapKey for QueueKey {
  type Value = ThreadSafeGuildIdMap<ThreadSafeQueue>;
}

pub struct VoiceManager;

impl TypeMapKey for VoiceManager {
  type Value = Arc<Mutex<ClientVoiceManager>>;
}

const SUPPORTED_HOSTS: &[&str] = &[
  "youtube.com",
  "youtu.be",
  "soundcloud.com",
  "www.soundcloud.com",
  "www.youtube.com",
  "www.youtu.be",
];
const SUPPORTED_HOSTS_SUFFIXES: &[&str] = &["bandcamp.com"];

fn is_youtube_dl_capable_host(host: &str) -> bool {
  let lower = host.to_lowercase();
  for host in SUPPORTED_HOSTS {
    if lower == *host {
      return true;
    }
  }

  for host in SUPPORTED_HOSTS_SUFFIXES {
    if lower.ends_with(host) {
      return true;
    }
  }

  false
}

fn youtube_dl_process_audio(track: ThreadSafeTrackStatus, queue_sender: QueueWorkerSender) {
  use std::process::Command;
  if let Some((source, url)) = track.get_info() {
    match tempfile::NamedTempFile::new() {
      Ok(temp_file) => {
        log::info!("temp file made at: {:?}", temp_file.path());
        track.set_state(TrackState::Downloading(source.clone()));
        log::info!("Now doanloading");
        let mut download_command = Command::new("youtube-dl");
        let ytdl_args = &[
          "-f",
          "bestaudio",
          "--no-continue",
          &*url,
          "-o",
          temp_file
            .path()
            .to_str()
            .expect("Temporary file path is not valid unicode"),
        ];
        download_command.args(ytdl_args);
        log::info!(
          "Running command: youtube-dl {}",
          ytdl_args.to_vec().join(" ")
        );
        match download_command.output() {
          Ok(output) => {
            if output.status.success() {
              log::info!("FInished downloading...");
              // persist to music dir
              let music_dir = std::path::Path::new("saved_tracks/");
              if !music_dir.exists() {
                if let Err(why) = std::fs::create_dir_all(&music_dir) {
                  log::error!("Failed to create saved tracks dir: {}", &why);
                  track.fail(Box::new(why));
                  return;
                }
              }
              let final_location = music_dir.join(source.id());
              if let Err(why) = temp_file.persist(&final_location) {
                log::error!("Failed to persist temp file to saved tracks dir: {}", &why);
                track.fail(Box::new(why));
                return;
              }
              log::info!("Track persisted to {:?}", &final_location);
              track.ready(final_location);
              if let Err(why) = queue_sender.send(QueueWorkerEvent::SongFinishedDownloading) {
                log::error!("Failed to send SongFinishedDownloading event: {}", &why);
              }
            } else {
              log::error!("Output status != success");
            }
          }
          Err(err) => {
            log::error!("Failed to spawn youtube-dl: {}", &err);
            track.fail(Box::new(err));
          }
        }
      }
      Err(err) => {
        log::error!("Failed to create temporary file: {}", &err);
        track.fail(Box::new(err));
      }
    }
  }
}

impl std::fmt::Display for Queue {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let mut buf = String::new();
    if let Some(ref connected_channel) = self.connected_channel {
      buf.push_str(&*format!(
        "Currently connected to: {}\n",
        connected_channel.channel.read()
      ));
    }
    if let Some(ref now_playing) = self.currently_playing {
      let status = if self.paused { "Paused on" } else { "Playing" };
      buf.push_str(&*format!("**{}** {}\n", status, now_playing.source))
    } else {
      buf.push_str("Nothing currently playing\n");
    }
    if !self.tracks.is_empty() {
      for (idx, track) in self.tracks.iter().enumerate() {
        match track.state.read() {
          Ok(track_state) => {
            buf.push_str(&*format!("**{}**: {}\n", idx + 1, track_state));
          }
          Err(why) => {
            buf.push_str(&*format!(
              "**{}**: Failed to read track state: {}",
              idx + 1,
              why
            ));
          }
        }
      }
    } else {
      buf.push_str("No queued tracks")
    }
    write!(f, "{}", buf)
  }
}

impl Queue {
  fn can_pop(&mut self) -> bool {
    if let Some(state) = self.tracks.first() {
      if let Ok(guard) = state.state.read() {
        if let TrackState::Ready { .. } = *guard {
          return true;
        }
      }
    }
    false
  }

  fn add_track(&mut self, url: &str) -> CommandResult {
    log::info!("Parsing {:?}", url);
    let url = url::Url::parse(url)?;
    let host_str = url.host_str();
    match host_str {
      Some(host) => {
        if is_youtube_dl_capable_host(host) {
          let source = TrackSource::from_url(url);
          // check if video is already saved...
          let cached_path = std::path::Path::new("saved_tracks/").join(source.id());
          if cached_path.exists() {
            log::info!("Cache hit on {}", source.id());
            let state = ThreadSafeTrackStatus {
              state: Arc::new(RwLock::new(TrackState::Ready {
                source,
                path: cached_path,
              })),
            };
            self.tracks.push(state);
            if let Err(why) = self
              .worker_sender
              .send(QueueWorkerEvent::SongFinishedDownloading)
            {
              log::error!(
                "Failed to notify worker thread of cached readied song: {:?}",
                &why
              );
            }
          } else {
            let state = ThreadSafeTrackStatus {
              state: Arc::new(RwLock::new(TrackState::Queueed(source))),
            };
            {
              let process_clone = state.clone();
              let worker_sender_clone = self.worker_sender.clone();
              self.tracks.push(state);
              self
                .runtime
                .lock()
                .execute(move || youtube_dl_process_audio(process_clone, worker_sender_clone));
            }
          }
        } else {
          return Err(CommandError(format!("Unsupported host: {}", host)));
        }
      }
      None => return Err(CommandError(String::from("No host provided"))),
    }
    Ok(())
  }

  fn pause(&mut self) -> CommandResult {
    self.paused = true;
    if let Some(ref now_playing) = self.currently_playing {
      now_playing.audio.lock().pause();
    }
    if let Err(why) = self.worker_sender.send(QueueWorkerEvent::StateUpdate) {
      log::error!("Failed to send state update: {}", &why);
    }
    Ok(())
  }

  fn resume(&mut self) -> CommandResult {
    self.paused = false;
    if let Some(ref now_playing) = self.currently_playing {
      now_playing.audio.lock().play();
    }
    if let Err(why) = self.worker_sender.send(QueueWorkerEvent::StateUpdate) {
      log::error!("Failed to send state update: {}", &why);
    }
    Ok(())
  }

  // if there are no songs in queue, do nothing
  // if there is a song available, move it to currently_playing
  fn next_song(&mut self) -> CommandResult {
    // if there is a currently playing song, yeet that mf
    if let Some(now_playing) = self.currently_playing.take() {
      // stop playing
      now_playing.audio.lock().pause();
    }
    // no other variant than Ready can be returned
    if !self.tracks.is_empty() {
      if let Some(TrackState::Ready { source, path }) = self.get_next_track() {
        if let Some(ref current_channel) = self.connected_channel {
          match voice::ffmpeg(path) {
            Ok(audio_source) => {
              let audio = current_channel.handler.lock().play_only(audio_source);
              self.currently_playing =
                Some(NowPlaying::new(source, audio, self.worker_sender.clone()));
            }
            Err(why) => {
              log::error!("Failed to construct audio source: {}", &why);
            }
          }
        }
      }
    }
    Ok(())
  }

  fn get_next_track(&mut self) -> Option<TrackState> {
    let first_track_ref = self.tracks.get(0);
    if let Some(track_state) = first_track_ref {
      let read_guard = track_state.state.read();
      if let Ok(track_state_read) = &read_guard {
        if let TrackState::Ready { .. } = **track_state_read {
          drop(first_track_ref);
          drop(read_guard);
          // remove from arc
          match Arc::try_unwrap(self.tracks.remove(0).state).map(|rw_lock| rw_lock.into_inner()) {
            Ok(Ok(state_lock)) => Some(state_lock),
            Ok(Err(why)) => {
              log::error!("Failed to remove state from RwLock: {}", &why);
              None
            }
            Err(arc) => {
              log::error!(
                "Song Arc still has {} strong references left when attempting to pop from queue",
                Arc::strong_count(&arc)
              );
              None
            }
          }
        } else {
          None
        }
      } else {
        None
      }
    } else {
      None
    }
  }
}

#[derive(Clone, Debug, Hash)]
enum TrackSource {
  YouTube {
    url: url::Url,
    embed_info: Option<EmbedInfo>,
  },
  SoundCloud {
    url: url::Url,
    embed_info: Option<EmbedInfo>,
  },
  Unknown(url::Url),
}

impl TrackSource {
  fn id(&self) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    self.url().hash(&mut hasher);
    format!("{:X}", hasher.finish())
  }
}

impl std::fmt::Display for TrackSource {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      TrackSource::YouTube { embed_info, url } | TrackSource::SoundCloud { embed_info, url } => {
        write!(
          f,
          "{}",
          if let Some(ref info) = embed_info {
            format!("[{}]({})", &info.title, &url)
          } else {
            url.to_string()
          }
        )
      }
      _ => write!(f, "{:?}", self),
    }
  }
}

impl TrackSource {
  fn from_url(url: url::Url) -> TrackSource {
    match url.host_str() {
      Some("youtube") | Some("youtu.be") | Some("www.youtube.com") | Some("www.youtu.be") => {
        let embed_info: Option<EmbedInfo> =
          reqwest::blocking::get(&*format!("https://noembed.com/embed?url={}", url))
            .ok()
            .map(|resp| resp.json::<EmbedInfo>().ok())
            .flatten();
        TrackSource::YouTube { url, embed_info }
      }
      Some("soundcloud.com") | Some("www.soundcloud.com") => {
        let embed_info: Option<EmbedInfo> =
          reqwest::blocking::get(&*format!("https://noembed.com/embed?url={}", url))
            .ok()
            .map(|resp| resp.json::<EmbedInfo>().ok())
            .flatten();
        TrackSource::SoundCloud { url, embed_info }
      }
      Some(_other_host) => TrackSource::Unknown(url),
      None => panic!("This cannot happen"),
    }
  }

  fn url(&self) -> String {
    match self {
      TrackSource::SoundCloud { url, .. } => url.to_string(),
      TrackSource::YouTube { url, .. } => url.to_string(),
      TrackSource::Unknown(url) => url.to_string(),
    }
  }
}

enum TrackState {
  Queueed(TrackSource),
  Downloading(TrackSource),
  Ready {
    source: TrackSource,
    path: std::path::PathBuf,
  },
  Failed {
    source: TrackSource,
    error: Box<dyn std::error::Error + Send + Sync>,
  },
}

impl fmt::Display for TrackState {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      TrackState::Failed { source, error } => write!(f, "**Failed** {}, {}", source, error),
      TrackState::Ready { source, .. } => write!(f, "{}", source),
      TrackState::Queueed(source) => write!(f, "**Queued & Processing** {}", source),
      TrackState::Downloading(source) => write!(f, "**Downloading** {}", source),
    }
  }
}

impl TrackState {
  fn source(&self) -> TrackSource {
    match self {
      TrackState::Queueed(s) => s,
      TrackState::Downloading(s) => s,
      TrackState::Ready { source, .. } => source,
      TrackState::Failed { source, .. } => source,
    }
    .clone()
  }

  fn url(&self) -> String {
    self.source().url()
  }
}

#[group]
#[commands(enqueue, list_queue, join, leave, skip, start, stop)]
struct Audio;

#[command]
#[aliases("queue", "tracks", "q")]
fn list_queue(ctx: &mut Context, msg: &Message) -> CommandResult {
  if let Ok(Channel::Guild(guild_chan)) = msg.channel_id.to_channel(&ctx.http) {
    let guild_chan_read_guard = guild_chan.read();
    let queue = get_queue_for_guild(ctx, guild_chan_read_guard.guild_id)?;
    let write_guard = queue.write();
    if let Ok(mut queue) = write_guard {
      let message = send_message(
        ctx,
        &guild_chan.read(),
        MessageParams {
          message: Some(format!("{}", queue)),
          ..MessageParams::default()
        },
      )
      .map_err(|e| CommandError(format!("Failed to send message: {:?}", e)))?;
      queue.last_queue_message = Some(LastQueueMessage {
        channel: Arc::clone(&guild_chan),
        message,
        sent_at: std::time::Instant::now(),
      });
      Ok(())
    } else {
      Err(CommandError(String::from("Could not acquire queue lock")))
    }
  } else {
    log::error!(
      "Failed to send message to non guild channel: {:?}",
      &msg.channel_id
    );
    Err(CommandError(String::from("Not in a guild channel")))
  }
}

#[command]
#[aliases("play", "p")]
fn enqueue(ctx: &mut Context, msg: &Message, args: Args) -> CommandResult {
  let guild = msg
    .guild(&ctx.cache)
    .ok_or_else(|| CommandError(String::from("Only Guilds supported")))?;
  let guild = guild.read();
  let queue = get_queue_for_guild(ctx, guild.id)?;
  if let Some(arg) = args.current() {
    let mut queue = queue
      .write()
      .map_err(|e| format!("Failed to acquire write lock for audio queue: {}", e))?;
    queue.add_track(arg)?;
    Ok(())
  } else {
    Err(CommandError(String::from("No link provided")))
  }
}

#[command]
#[aliases("summon")]
fn join(ctx: &mut Context, msg: &Message) -> CommandResult {
  let voice_manager_lock = ctx
    .data
    .read()
    .get::<VoiceManager>()
    .cloned()
    .ok_or_else(|| CommandError(String::from("Could not find VoiceManager")))?;
  let mut voice_manager = voice_manager_lock.lock();
  let guild = msg
    .guild(&ctx.cache)
    .ok_or_else(|| CommandError(String::from("Only Guilds supported")))?;
  let guild = guild.read();
  let channel_id = guild
    .voice_states
    .get(&msg.author.id)
    .and_then(|chan| chan.channel_id)
    .ok_or_else(|| CommandError(String::from("You are not in a voice channel")))?;
  if let Some(Channel::Guild(channel)) = msg.channel(&ctx.cache) {
    // this match won't fail
    match voice_manager.join(&guild.id, channel_id) {
      Some(handler) => {
        if let Ok(mut queue) = get_queue_for_guild(ctx, guild.id)?.write() {
          let handler = Arc::new(Mutex::new(handler.clone()));
          queue.connected_channel = Some(VoiceConnection { handler, channel });
          if queue.can_pop() {
            if let Err(why) = queue.next_song() {
              log::error!("Failed to increment song queue: {:?}", &why);
            }
            if let Err(why) = queue.worker_sender.send(QueueWorkerEvent::StateUpdate) {
              log::error!("Failed to send state update: {}", &why);
            }
          }
        }
      }
      None => {
        return Err(CommandError(format!("Failed to join {}", channel_id)));
      }
    }
  }
  Ok(())
}

#[command]
#[aliases("unpause", "resume")]
fn start(ctx: &mut Context, msg: &Message) -> CommandResult {
  // if there was a song playing, resume it
  // otherwise, if there is a song ready to be played, play it
  // otherwise, do nothing
  let guild = msg
    .guild(&ctx.cache)
    .ok_or_else(|| CommandError(String::from("Only Guilds supported")))?;
  let guild = guild.read();
  let guild_id = guild.id;
  drop(guild);
  let queue_lock = get_queue_for_guild(ctx, guild_id)?;
  let mut queue = queue_lock
    .write()
    .map_err(|e| CommandError(format!("Failed to get queue write lock: {}", &e)))?;
  queue.resume()
}

#[command]
#[aliases("pause")]
fn stop(ctx: &mut Context, msg: &Message) -> CommandResult {
  // set queue status to paused
  // if there is a current song playing, pause it
  let guild = msg
    .guild(&ctx.cache)
    .ok_or_else(|| CommandError(String::from("Only Guilds supported")))?;
  let guild = guild.read();
  let guild_id = guild.id;
  drop(guild);
  let queue_lock = get_queue_for_guild(ctx, guild_id)?;
  let mut queue = queue_lock
    .write()
    .map_err(|e| CommandError(format!("Failed to get queue write lock: {}", &e)))?;
  queue.pause()
}

#[command]
fn skip(ctx: &mut Context, msg: &Message) -> CommandResult {
  let guild = msg
    .guild(&ctx.cache)
    .ok_or_else(|| CommandError(String::from("Only Guilds supported")))?;
  let guild = guild.read();
  let guild_id = guild.id;
  drop(guild);
  let queue_lock = get_queue_for_guild(ctx, guild_id)?;
  let mut queue = queue_lock
    .write()
    .map_err(|e| CommandError(format!("Failed to get queue write lock: {}", &e)))?;
  queue.next_song()?;
  if let Err(why) = queue.worker_sender.send(QueueWorkerEvent::StateUpdate) {
    log::error!("Failed to send queue state update: {}", &why);
  }
  Ok(())
}

// 1. Find voice manager for guild -> leave
// 2. Find queue for guild -> set connected_channel to None
#[command]
fn leave(ctx: &mut Context, msg: &Message) -> CommandResult {
  let guild = msg
    .guild(&ctx.cache)
    .ok_or_else(|| CommandError(String::from("Only Guilds supported")))?;
  let guild = guild.read();
  let voice_manager_lock = ctx
    .data
    .read()
    .get::<VoiceManager>()
    .cloned()
    .ok_or_else(|| CommandError(String::from("Could not find VoiceManager")))?;
  let mut voice_manager = voice_manager_lock.lock();
  voice_manager
    .leave(&guild.id)
    .ok_or_else(|| CommandError(String::from("Failed to leave channel")))?;
  let queue = get_queue_for_guild(ctx, guild.id)?;
  if let Ok(mut queue_write_guard) = queue.write() {
    queue_write_guard.connected_channel = None;
  }
  Ok(())
}

fn get_queue_for_guild(ctx: &Context, guild_id: GuildId) -> Result<ThreadSafeQueue, CommandError> {
  let read_ctx = ctx.data.read();
  let runtime = read_ctx
    .get::<ThreadPoolKey>()
    .ok_or_else(|| CommandError(String::from("Could not find Audio Runtime in Context")))?
    .clone();
  drop(read_ctx);
  let mut data_write = ctx.data.write();
  let mut queue_map = data_write
    .get_mut::<QueueKey>()
    .ok_or_else(|| CommandError(String::from("Could not find Queue map in Context")))?
    .write()
    .map_err(|e| CommandError(format!("Could not acquire write lock on Queue map: {}", e)))?;
  // TODO(haze): optimize (don't call for bot id every time)
  let bot_id = ctx
    .http
    .get_current_user()
    .map_err(|e| CommandError(format!("Could not get Bot Id: {}", &e)))?
    .id;
  let entry = queue_map
    .entry(guild_id)
    .or_insert_with(|| Queue::new(runtime, bot_id, ctx.cache.clone(), Arc::clone(&ctx.http)));
  Ok(Arc::clone(&entry))
}
