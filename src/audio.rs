use crate::{common::*, error::StringError};
use crossbeam::channel;
use serde::Deserialize;
use serenity::{
  cache::Cache,
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
  prelude::{RwLock, *},
  voice,
};
use std::{collections::HashMap, fmt, sync::Arc};

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
  async fn set_state(&self, state: TrackState) {
    *self.state.write().await = state;
  }

  async fn get_url(&self) -> String {
    self.state.read().await.url()
  }

  async fn get_source(&self) -> TrackSource {
    self.state.read().await.source()
  }

  async fn ready(&self, path: std::path::PathBuf) {
    let source = self.get_source().await;
    self.set_state(TrackState::Ready { source, path }).await;
  }

  async fn fail<E: 'static>(&self, error: Box<E>)
  where
    E: std::error::Error + Send + Sync,
  {
    let source = self.get_source().await;
    self.set_state(TrackState::Failed { source, error }).await;
  }

  async fn fail_str(&self, error: String) {
    let source = self.get_source().await;
    self
      .set_state(TrackState::SimpleFailed { source, error })
      .await;
  }

  // (Source, Url)
  async fn get_info(&self) -> (TrackSource, String) {
    let state_read = self.state.read().await;
    (state_read.source(), state_read.url())
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
  worker_handle: tokio::task::JoinHandle<()>,
}

type QueueWorkerSender = channel::Sender<QueueWorkerEvent>;
type QueueWorkerReceiver = channel::Receiver<QueueWorkerEvent>;

async fn now_playing_task(queue_sender: QueueWorkerSender, audio: voice::LockedAudio) {
  loop {
    if audio.lock().await.finished {
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
      worker_handle: tokio::spawn(now_playing_task(queue_sender, worker_clone)),
    }
  }
}

struct VoiceConnection {
  channel: GuildChannel,
  handler: Arc<Mutex<voice::Handler>>,
}

pub struct Queue {
  currently_playing: Option<NowPlaying>,
  connected_channel: Option<VoiceConnection>,
  tracks: Vec<ThreadSafeTrackStatus>,
  worker_sender: QueueWorkerSender,
  last_queue_message: Option<LastQueueMessage>,
  bot_id: UserId,
  cache: Arc<Cache>,
  http: Arc<Http>,
  paused: bool,
  pub volume: f32,
}

struct LastQueueMessage {
  sent_at: std::time::Instant,
  message: Message,
  channel: GuildChannel,
}

// 1. if there is nothing playing, and a song just finished downloading, try to begin playing
// 2. if a song finished playing, try to play the next song
// 3. comb queue of failed tracks before!
async fn queue_worker_task(queue: Arc<RwLock<Queue>>, receiver: QueueWorkerReceiver) {
  async fn next_song(q: &Arc<RwLock<Queue>>) {
    let mut queue = q.write().await;
    queue.clear_failed_tracks().await;
    if let Err(why) = queue.next_song().await {
      log::error!("Can't play next song: {:?}", &why);
    }
    queue.state_update();
  }
  for event in receiver {
    let queue_read = queue.read().await;
    let is_playing_anything = queue_read.currently_playing.is_some();
    let can_play = queue_read.connected_channel.is_some() && !queue_read.paused;
    drop(queue_read);
    {
      // every time we receive an event, update the last queue message (if any)
      let queue_write = queue.write().await;
      let bot_id = queue_write.bot_id;
      if let Some(ref last_msg) = queue_write.last_queue_message {
        let mut msg = last_msg.message.clone();
        if let Ok(perms) = last_msg
          .channel
          .permissions_for_user(&queue_write.cache, bot_id)
          .await
        {
          let queue_string = queue_write.to_string().await;
          if let Err(why) = msg
            .edit(&queue_write.http, |mut cm| {
              crate::common::simple_message_edit(
                &mut cm,
                MessageParams {
                  message: Some(queue_string),
                  ..MessageParams::default()
                },
                perms,
              );
              cm
            })
            .await
          {
            log::error!("Error updating last queue message: {}", &why);
          }
        }
      }
    }
    match event {
      QueueWorkerEvent::SongFinished => {
        next_song(&queue).await;
      }
      QueueWorkerEvent::SongFinishedDownloading => {
        // play next song, if nothing is playing
        if !is_playing_anything && can_play {
          next_song(&queue).await;
        }
      }
      _ => {}
    }
  }
}

impl Queue {
  pub async fn new(
    bot_id: UserId,
    cache: Arc<Cache>,
    http: Arc<Http>,
    volume: Option<f32>,
  ) -> Arc<RwLock<Queue>> {
    let (worker_sender, receiver) = channel::unbounded();
    let queue = Arc::new(RwLock::new(Queue {
      currently_playing: None,
      connected_channel: None,
      tracks: Vec::new(),
      worker_sender,
      last_queue_message: None,
      paused: false,
      cache,
      http,
      bot_id,
      volume: volume.unwrap_or_else(|| 1_f32),
    }));
    let queue_clone = Arc::clone(&queue);
    tokio::spawn(queue_worker_task(queue_clone, receiver));
    queue
  }
}

type ThreadSafeQueue = Arc<RwLock<Queue>>;
type ThreadSafeGuildIdMap<T> = Arc<RwLock<HashMap<GuildId, T>>>;

pub struct YouTubeApiKey;
impl TypeMapKey for YouTubeApiKey {
  type Value = Arc<String>;
}

pub struct SoundCloudApiKey;
impl TypeMapKey for SoundCloudApiKey {
  type Value = Arc<String>;
}

pub struct SearchClientKey;
impl TypeMapKey for SearchClientKey {
  type Value = Arc<reqwest::Client>;
}

pub struct VolumeMapKey;
impl TypeMapKey for VolumeMapKey {
  type Value = Arc<HashMap<u64, f32>>;
}

pub struct QueueKey;
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

async fn youtube_dl_process_audio(track: ThreadSafeTrackStatus, queue_sender: QueueWorkerSender) {
  use std::process::Command;
  let (source, url) = track.get_info().await;
  let music_dir = std::path::Path::new("saved_tracks/");
  if !music_dir.exists() {
    if let Err(why) = std::fs::create_dir_all(&music_dir) {
      log::error!("Failed to create saved tracks dir: {}", &why);
      track.fail(Box::new(why)).await;
      return;
    }
  }
  let file_path = music_dir.join(&source.id());
  match std::fs::File::create(&file_path) {
    Ok(_new_file) => {
      log::info!("Audio file made at: {:?}", &file_path);
      track
        .set_state(TrackState::Downloading(source.clone()))
        .await;
      log::info!("Beginning download");
      let mut download_command = Command::new("youtube-dl");
      let ytdl_args = &[
        "-f",
        "bestaudio",
        "--no-continue",
        "--rm-cache-dir",
        &*url,
        "-o",
        file_path
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
            track.ready(file_path).await;
            if let Err(why) = queue_sender.send(QueueWorkerEvent::SongFinishedDownloading) {
              log::error!("Failed to send SongFinishedDownloading event: {}", &why);
            }
          } else {
            if let Ok(utf8_stdout) = String::from_utf8(output.stdout) {
              track
                .fail_str(format!("Download command failed: {}", &utf8_stdout))
                .await;
            } else {
              log::error!("Command output was not UTF8, but the download was a failure :(");
            }
          }
        }
        Err(err) => {
          log::error!("Failed to spawn youtube-dl: {}", &err);
          track.fail(Box::new(err)).await;
        }
      }
    }
    Err(err) => {
      log::error!("Failed to create temporary file: {}", &err);
      track.fail(Box::new(err)).await;
    }
  }
}

impl Queue {
  /// Clears the queue
  fn clear(&mut self) {
    self.tracks.clear();
  }

  async fn to_string(&self) -> String {
    let mut buf = String::new();
    if let Some(ref connected_channel) = self.connected_channel {
      buf.push_str(&*format!(
        "Currently connected to: {}\n",
        connected_channel.channel
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
        let track_state = track.state.read().await;
        buf.push_str(&*format!("**{}**: {}\n", idx + 1, *track_state));
      }
    } else {
      buf.push_str("No queued tracks")
    }
    buf
  }

  async fn clear_failed_tracks(&mut self) {
    // ugh, no async retain
    let mut track_ids_to_remove = Vec::new();
    for (idx, track) in self.tracks.iter().enumerate() {
      let state = track.state.read().await;
      if let TrackState::Failed { .. } = *state {
        track_ids_to_remove.push(idx);
      }
    }
    for idx in track_ids_to_remove {
      self.tracks.remove(idx);
    }
  }

  fn state_update(&self) {
    self.send_worker_event(QueueWorkerEvent::StateUpdate);
  }

  fn send_worker_event(&self, event: QueueWorkerEvent) {
    if let Err(why) = self.worker_sender.send(event) {
      log::error!("Failed to send worker event: {:?}, {}", event, why);
    }
  }

  async fn set_volume(&mut self, new_volume: f32) {
    self.volume = new_volume;
    if let Some(ref now_playing) = self.currently_playing {
      now_playing.audio.lock().await.volume(self.volume);
    }
  }

  async fn can_pop(&mut self) -> bool {
    if let Some(state) = self.tracks.first() {
      let guard = state.state.read().await;
      if let TrackState::Ready { .. } = *guard {
        return true;
      }
    }
    false
  }

  async fn add_track(&mut self, url: url::Url) -> Result<(), StringError> {
    let host_str = url.host_str();
    dbg!(&host_str);
    match host_str {
      Some(host) => {
        if is_youtube_dl_capable_host(host) {
          let source = TrackSource::from_url(url).await;
          // check if video is already saved...
          let cached_path = std::path::Path::new("saved_tracks/").join(source.id());
          log::info!("cached path = {:?}", &cached_path);
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
            log::info!("no cache hit lol");
            let state = ThreadSafeTrackStatus {
              state: Arc::new(RwLock::new(TrackState::Queueed(source))),
            };
            {
              let process_clone = state.clone();
              let worker_sender_clone = self.worker_sender.clone();
              log::info!("before dl process audio task");
              self.tracks.push(state);
              tokio::spawn(youtube_dl_process_audio(process_clone, worker_sender_clone));
            }
          }
        } else {
          return Err(StringError(format!("Unsupported host: {}", host)));
        }
      }
      None => return Err(StringError::from("No host provided")),
    }
    Ok(())
  }

  async fn pause(&mut self) -> CommandResult {
    self.paused = true;
    if let Some(ref now_playing) = self.currently_playing {
      now_playing.audio.lock().await.pause();
    }
    if let Err(why) = self.worker_sender.send(QueueWorkerEvent::StateUpdate) {
      log::error!("Failed to send state update: {}", &why);
    }
    Ok(())
  }

  async fn resume(&mut self) -> CommandResult {
    self.paused = false;
    if let Some(ref now_playing) = self.currently_playing {
      now_playing.audio.lock().await.play();
    }
    if let Err(why) = self.worker_sender.send(QueueWorkerEvent::StateUpdate) {
      log::error!("Failed to send state update: {}", &why);
    }
    Ok(())
  }

  // if there are no songs in queue, do nothing
  // if there is a song available, move it to currently_playing
  async fn next_song(&mut self) -> CommandResult {
    // if there is a currently playing song, yeet that mf
    if let Some(now_playing) = self.currently_playing.take() {
      // stop playing
      now_playing.audio.lock().await.pause();
    }
    // no other variant than Ready can be returned
    if !self.tracks.is_empty() {
      if let Some(TrackState::Ready { source, path }) = self.get_next_track().await {
        if let Some(ref current_channel) = self.connected_channel {
          match voice::ffmpeg(path).await {
            Ok(audio_source) => {
              let audio = current_channel.handler.lock().await.play_only(audio_source);
              audio.lock().await.volume(self.volume);
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

  async fn get_next_track(&mut self) -> Option<TrackState> {
    let first_track_ref = self.tracks.get(0);
    if let Some(track_state) = first_track_ref {
      let read_guard = track_state.state.read().await;
      if let TrackState::Ready { .. } = *read_guard {
        drop(first_track_ref);
        drop(read_guard);
        // remove from arc
        match Arc::try_unwrap(self.tracks.remove(0).state).map(|rw_lock| rw_lock.into_inner()) {
          Ok(state_lock) => Some(state_lock),
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
    match self {
      TrackSource::SoundCloud {
        embed_info: Some(EmbedInfo {
          thumbnail_url: Some(url),
          ..
        }),
        ..
      }
      | TrackSource::YouTube {
        embed_info: Some(EmbedInfo {
          thumbnail_url: Some(url),
          ..
        }),
        ..
      } => url.hash(&mut hasher),
      _ => self.url().hash(&mut hasher),
    }
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
  async fn from_url(url: url::Url) -> TrackSource {
    match url.host_str() {
      Some("youtube")
      | Some("youtu.be")
      | Some("youtube.com")
      | Some("www.youtube.com")
      | Some("www.youtu.be") => {
        let embed_info: Option<EmbedInfo> = if let Some(response) =
          reqwest::get(&*format!("https://noembed.com/embed?url={}", url))
            .await
            .ok()
        {
          response.json::<EmbedInfo>().await.ok()
        } else {
          None
        };
        TrackSource::YouTube { url, embed_info }
      }
      Some("soundcloud.com") | Some("www.soundcloud.com") => {
        let embed_info: Option<EmbedInfo> = if let Some(response) =
          reqwest::get(&*format!("https://noembed.com/embed?url={}", url))
            .await
            .ok()
        {
          response.json::<EmbedInfo>().await.ok()
        } else {
          None
        };

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
  SimpleFailed {
    source: TrackSource,
    error: String,
  },
}

impl fmt::Display for TrackState {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      TrackState::Failed { source, error } => write!(f, "**Failed** {}, {}", source, error),
      TrackState::SimpleFailed { source, error } => write!(f, "**Failed** {}, {}", source, error),
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
      TrackState::SimpleFailed { source, .. } => source,
    }
    .clone()
  }

  fn url(&self) -> String {
    self.source().url()
  }
}

#[group]
#[commands(
  enqueue,
  list_queue,
  join,
  leave,
  skip,
  start,
  stop,
  search,
  volume,
  wipe_audio_cache,
  clear,
  remove
)]
struct Audio;

#[command]
async fn wipe_audio_cache(ctx: &Context, msg: &Message, args: Args) -> CommandResult {
  let (total_bytes, items): (u64, usize) = std::fs::read_dir("saved_tracks/")
    .map_err(|e| StringError(format!("Failed to read track cache: {}", e)))?
    .fold((0, 0), |(mut bytes, mut items), entry| {
      match entry {
        Ok(file) => {
          match file.metadata() {
            Ok(meta) => bytes += meta.len(),
            Err(why) => log::error!("Failed to read specific cached track metadata: {}", &why),
          };
          items += 1;
        }
        Err(why) => log::error!("Failed to read specific cached track: {}", &why),
      };
      (bytes, items)
    });
  match args.current() {
    Some("--dry") => {} // dry run
    _ => {
      std::fs::remove_dir_all("saved_tracks/")
        .map_err(|e| StringError(format!("Failed to remove cached track dir: {}", e)))?;
      std::fs::create_dir_all("saved_tracks/")
        .map_err(|e| StringError(format!("Failed to create cached track dir: {}", e)))?;
    }
  }
  match msg.channel(&ctx).await {
    Some(Channel::Guild(guild_chan)) => {
      send_message(
        ctx,
        &guild_chan,
        MessageParams {
          message: Some(format!(
            "Wiped **{}** files, totaling **{}**",
            items,
            pretty_bytes::converter::convert(total_bytes as f64)
          )),
          ..MessageParams::default()
        },
      )
      .await
      .map_err(|e| StringError(format!("Failed to send message: {:?}", e)))?;
    }
    _ => {
      if let Err(why) = msg.reply(ctx, "Can only be used in Guild channels").await {
        log::error!("Failed to send error message: {}", &why);
      }
    }
  };
  Ok(())
}

#[command]
#[aliases("vol", "v")]
async fn volume(ctx: &Context, msg: &Message, args: Args) -> CommandResult {
  let ctx_read = ctx.data.read().await;
  let bot_id = ctx_read.get::<crate::BotIdKey>().unwrap().clone();
  drop(ctx_read);

  let guild = msg
    .guild(&ctx.cache)
    .await
    .ok_or_else(|| StringError::from("Only Guilds supported"))?;
  let guild_id = guild.id;
  drop(guild);
  let queue_lock = get_queue_for_guild(ctx, guild_id, bot_id).await?;
  let queue_read = queue_lock.read().await;
  let current_volume = queue_read.volume;
  drop(queue_read);
  let mut queue_write = queue_lock.write().await;
  let response = match args.parse::<f32>() {
    Ok(new_volume) => {
      queue_write.set_volume(new_volume / 100_f32).await;
      format!(
        "Volume changed from **{:.0}** to **{:.0}**",
        &current_volume * 100_f32,
        &new_volume,
      )
    }
    Err(_) => format!("Volume is currently **{:.0}**", &current_volume * 100_f32),
  };
  match msg.channel(&ctx).await {
    Some(Channel::Guild(guild_chan)) => {
      send_message(
        ctx,
        &guild_chan,
        MessageParams {
          message: Some(response),
          ..MessageParams::default()
        },
      )
      .await
      .map_err(|e| StringError(format!("Failed to send message: {:?}", e)))?;
    }
    _ => {
      if let Err(why) = msg.reply(ctx, "Can only be used in Guild channels").await {
        log::error!("Failed to send error message: {}", &why);
      }
    }
  };
  Ok(())
}

#[command]
#[aliases("s")]
async fn search(ctx: &Context, msg: &Message, mut args: Args) -> CommandResult {
  use crate::search;
  match msg.channel(&ctx).await {
    // send help
    Some(chan) => match chan {
      Channel::Guild(guild_chan) => {
        if args.is_empty() {
          send_message(
            ctx,
            &guild_chan,
            MessageParams {
              message: Some(String::from(
                "usage: .s|.search <{youtube,yt}|{soundcloud,sc}> <query>",
              )),
              ..MessageParams::default()
            },
          )
          .await
          .map_err(|e| StringError(format!("Failed to send message: {:?}", e)))?;
        } else {
          let ctx_read = ctx.data.read().await;
          let search_client = ctx_read
            .get::<SearchClientKey>()
            .ok_or_else(|| StringError::from("Could not get Search HTTP Client"))?;
          let search_site = match args.parse::<search::Site>() {
            Ok(site) => {
              args.advance();
              site
            }
            Err(_) => search::Site::YouTube,
          };
          let query_result_str = match search_site {
            search::Site::YouTube => {
              // search youtube
              let api_key = ctx_read
                .get::<YouTubeApiKey>()
                .ok_or_else(|| StringError::from("Could not find Youtube API Key"))?;
              let search_results =
                search::youtube::search(search_client.as_ref(), args.rest(), &api_key)
                  .await
                  .map_err(|e| StringError(format!("Failed to search: {:?}", e)))?;
              format_results(search_results.as_slice())
            }
            search::Site::SoundCloud => {
              // search soundcloud
              let client_id = ctx_read
                .get::<SoundCloudApiKey>()
                .ok_or_else(|| StringError::from("Could not find SoundCloud API Key"))?;
              let search_results =
                search::soundcloud::search(search_client.as_ref(), args.rest(), &client_id)
                  .await
                  .map_err(|e| StringError(format!("Failed to search: {:?}", e)))?;
              format_results(search_results.as_slice())
            }
          };
          send_message(
            ctx,
            &guild_chan,
            MessageParams {
              title: Some(format!("**{:?}** Results", search_site)),
              message: Some(query_result_str),
              ..MessageParams::default()
            },
          )
          .await
          .map_err(|e| StringError(format!("Failed to send message: {:?}", e)))?;
        }
      }
      _ => {
        if let Err(why) = msg
          .reply(&ctx, "Needs to be executed in a guild channel")
          .await
        {
          log::error!("Failed to send message: {:?}", &why);
        }
      }
    },
    None => {}
  }
  Ok(())
}

fn format_results(search_results: &[crate::search::SearchResult]) -> String {
  if search_results.is_empty() {
    String::from("No results")
  } else {
    search_results
      .iter()
      .enumerate()
      .fold(String::new(), |mut acc, (idx, result)| {
        acc.push_str(&*format!(
          "**{}**: [{}]({}) by **{}**\n",
          idx + 1,
          result.title,
          result.url,
          result.author
        ));
        acc
      })
  }
}

#[command]
#[aliases("queue", "tracks", "q")]
async fn list_queue(ctx: &Context, msg: &Message) -> CommandResult {
  let ctx_read = ctx.data.read().await;
  let bot_id = ctx_read.get::<crate::BotIdKey>().unwrap().clone();
  drop(ctx_read);

  if let Ok(Channel::Guild(guild_chan)) = msg.channel_id.to_channel(&ctx.http).await {
    let queue = get_queue_for_guild(ctx, guild_chan.guild_id, bot_id).await?;
    let mut write_guard = queue.write().await;
    let queue_string = write_guard.to_string().await;
    let message = send_message(
      ctx,
      &guild_chan,
      MessageParams {
        message: Some(queue_string),
        ..MessageParams::default()
      },
    )
    .await
    .map_err(|e| StringError(format!("Failed to send message: {:?}", e)))?;
    write_guard.last_queue_message = Some(LastQueueMessage {
      channel: guild_chan,
      message,
      sent_at: std::time::Instant::now(),
    });
    Ok(())
  } else {
    log::error!(
      "Failed to send message to non guild channel: {:?}",
      &msg.channel_id
    );
    Err(StringError::from("Not in a guild channel").into())
  }
}

#[command]
#[aliases("play", "p")]
async fn enqueue(ctx: &Context, msg: &Message, mut args: Args) -> CommandResult {
  use crate::search;

  let ctx_read = ctx.data.read().await;
  let bot_id = ctx_read.get::<crate::BotIdKey>().unwrap().clone();
  drop(ctx_read);

  let guild = msg
    .guild(&ctx.cache)
    .await
    .ok_or_else(|| StringError::from("Only Guilds supported"))?;

  if let Ok(first_arg) = args.parse::<String>() {
    let lower = first_arg.to_lowercase();
    if &*lower == "--help" || &*lower == "-h" {
      if let Ok(Channel::Guild(guild_chan)) = msg.channel_id.to_channel(&ctx.http).await {
        return send_message(
          ctx,
          &guild_chan,
          MessageParams {
            message: Some(String::from(
              "`.play <sc|yt (default)> <index (default is 1)> <query>`",
            )),
            ..MessageParams::default()
          },
        )
        .await
        .map(|_| ())
        .map_err(|e| CommandError::from(StringError(format!("Failed to send message: {:?}", e))));
      } else {
        msg.reply(&ctx, "Play only works in guild channels").await?;
        return Ok(());
      }
    }
  }

  let queue_lock = get_queue_for_guild(ctx, guild.id, bot_id).await?;
  log::info!("got lock");
  let queue_read = queue_lock.read().await;
  log::info!("read from lock");
  let isnt_connected_to_channel = queue_read.connected_channel.is_none();
  drop(queue_read);
  drop(queue_lock);
  log::info!("got here");
  // to free up ctx ^
  let ctx_read = ctx.data.read().await;
  let soundcloud_client_id = ctx_read
    .get::<SoundCloudApiKey>()
    .ok_or_else(|| StringError(String::from("Could not find SoundCloud API Key")))?
    .clone();
  let youtube_api_key = ctx_read
    .get::<YouTubeApiKey>()
    .ok_or_else(|| StringError(String::from("Could not find Youtube API Key")))?
    .clone();
  let search_client = ctx_read
    .get::<SearchClientKey>()
    .ok_or_else(|| StringError(String::from("Could not get Search HTTP Client")))?
    .clone();
  if isnt_connected_to_channel {
    let voice_manager_lock = ctx_read
      .get::<VoiceManager>()
      .cloned()
      .ok_or_else(|| StringError(String::from("Could not find VoiceManager")))?;
    let mut voice_manager = voice_manager_lock.lock().await;
    let guild = msg
      .guild(&ctx.cache)
      .await
      .ok_or_else(|| StringError(String::from("Only Guilds supported")))?;
    let channel_id = guild
      .voice_states
      .get(&msg.author.id)
      .and_then(|chan| chan.channel_id)
      .ok_or_else(|| StringError(String::from("You are not in a voice channel")))?;
    if let Some(Channel::Guild(channel)) = msg.channel(&ctx.cache).await {
      // this match won't fail
      match voice_manager.join(&guild.id, channel_id) {
        Some(handler) => {
          drop(ctx_read);
          let queue_lock = get_queue_for_guild(ctx, guild.id, bot_id).await?;
          let mut queue = queue_lock.write().await;
          let handler = Arc::new(Mutex::new(handler.clone()));
          queue.connected_channel = Some(VoiceConnection { handler, channel });
        }
        None => {
          return Err(StringError(format!("Failed to join {}", channel_id)).into());
        }
      }
    }
  } else {
    drop(ctx_read);
  }
  // drop read ctx for queue write
  log::info!("after we drop ctx_read");
  // get queue lock back
  let queue_lock = get_queue_for_guild(ctx, guild.id, bot_id).await?;
  log::info!("got queue lock...");
  let mut queue = queue_lock.write().await;
  if let Ok(url) = args.parse::<url::Url>() {
    queue.add_track(url).await?;
    queue.state_update();
  } else {
    log::info!("beginning search");
    let search_site = match args.parse::<search::Site>() {
      Ok(site) => {
        args.advance();
        site
      }
      Err(_) => search::Site::YouTube,
    };
    let result_idx = match args.parse::<usize>() {
      Ok(idx) => {
        args.advance();
        idx
      }
      Err(_) => 1,
    };
    log::info!("before search results");
    let search_results = match search_site {
      search::Site::YouTube => {
        // search youtube
        search::youtube::search(search_client.as_ref(), args.rest(), &youtube_api_key)
          .await
          .map_err(|e| StringError(format!("Failed to search: {:?}", e)))?
      }
      search::Site::SoundCloud => {
        // search soundcloud
        search::soundcloud::search(search_client.as_ref(), args.rest(), &soundcloud_client_id)
          .await
          .map_err(|e| StringError(format!("Failed to search: {:?}", e)))?
      }
    };
    log::info!("results gotten");
    if search_results.is_empty() {
      if let Err(why) = msg.reply(&ctx, "No search results found").await {
        log::error!(
          "Failed to notify user that there were no search results: {}",
          &why
        );
      }
    } else {
      let select_idx = if result_idx - 1 > search_results.len() {
        0
      } else {
        result_idx - 1
      };
      let track = &search_results[select_idx];
      log::info!("before adding track");
      queue
        .add_track(url::Url::parse(&*track.url).map_err(|e| {
          StringError(format!(
            "Failed to parse URL for search result {}: {}",
            select_idx, e
          ))
        })?)
        .await?;
      queue.state_update();
    }
  }
  Ok(())
}

#[command]
async fn clear(ctx: &Context, msg: &Message) -> CommandResult {
  let ctx_read = ctx.data.read().await;
  let bot_id = ctx_read.get::<crate::BotIdKey>().unwrap().clone();
  drop(ctx_read);

  let guild = msg
    .guild(&ctx.cache)
    .await
    .ok_or_else(|| StringError::from("Only Guilds supported"))?;
  let guild_id = guild.id;
  let queue = get_queue_for_guild(ctx, guild_id, bot_id).await?;
  let mut queue_write_guard = queue.write().await;
  queue_write_guard.clear();
  Ok(())
}

#[command]
async fn remove(ctx: &Context, msg: &Message, mut args: Args) -> CommandResult {
  let ctx_read = ctx.data.read().await;
  let bot_id = ctx_read.get::<crate::BotIdKey>().unwrap().clone();
  drop(ctx_read);

  let guild = msg
    .guild(&ctx.cache)
    .await
    .ok_or_else(|| StringError::from("Only Guilds supported"))?;
  let guild_id = guild.id;
  let queue = get_queue_for_guild(ctx, guild_id, bot_id).await?;
  let remove_idx = args.single::<usize>().map_err(|e| {
    StringError(format!(
      "Invalid index provided ({}). Usage: .remove <idx>",
      e
    ))
  })?;
  let mut queue_write_guard = queue.write().await;
  let actual_remove_idx = remove_idx - 1;
  log::info!(
    "a={}, l={}",
    actual_remove_idx,
    queue_write_guard.tracks.len()
  );
  if !queue_write_guard.tracks.is_empty() {
    if actual_remove_idx < queue_write_guard.tracks.len() {
      queue_write_guard.tracks.remove(actual_remove_idx);
      queue_write_guard.state_update();
      log::info!("Successfully removed track @ idx {}", remove_idx);
    } else {
      msg
        .reply(
          &ctx,
          format!(
            "Invalid index, should be between 1 and {}",
            queue_write_guard.tracks.len()
          ),
        )
        .await?;
    }
  }
  Ok(())
}

#[command]
#[aliases("summon", "j")]
async fn join(ctx: &Context, msg: &Message) -> CommandResult {
  let ctx_read = ctx.data.read().await;
  let bot_id = ctx_read.get::<crate::BotIdKey>().unwrap().clone();
  drop(ctx_read);

  let voice_manager_lock = ctx
    .data
    .read()
    .await
    .get::<VoiceManager>()
    .cloned()
    .ok_or_else(|| CommandError::from(StringError::from("Could not find VoiceManager")))?;
  let mut voice_manager = voice_manager_lock.lock().await;
  let guild = msg
    .guild(&ctx.cache)
    .await
    .ok_or_else(|| CommandError::from(StringError::from("Only Guilds supported")))?;
  let channel_id = guild
    .voice_states
    .get(&msg.author.id)
    .and_then(|chan| chan.channel_id)
    .ok_or_else(|| StringError(String::from("You are not in a voice channel")))?;
  if let Some(Channel::Guild(channel)) = msg.channel(&ctx.cache).await {
    // this match won't fail
    match voice_manager.join(&guild.id, channel_id) {
      Some(handler) => {
        let queue_lock = get_queue_for_guild(ctx, guild.id, bot_id).await?;
        let mut queue = queue_lock.write().await;
        let handler = Arc::new(Mutex::new(handler.clone()));
        queue.connected_channel = Some(VoiceConnection { handler, channel });
        if queue.can_pop().await {
          if let Err(why) = queue.next_song().await {
            log::error!("Failed to increment song queue: {:?}", &why);
          }
          queue.state_update();
        }
      }
      None => {
        return Err(CommandError::from(StringError(format!(
          "Failed to join {}",
          channel_id
        ))));
      }
    }
  }
  Ok(())
}

#[command]
#[aliases("unpause", "resume")]
async fn start(ctx: &Context, msg: &Message) -> CommandResult {
  let ctx_read = ctx.data.read().await;
  let bot_id = ctx_read.get::<crate::BotIdKey>().unwrap().clone();
  drop(ctx_read);

  // if there was a song playing, resume it
  // otherwise, if there is a song ready to be played, play it
  // otherwise, do nothing
  let guild = msg
    .guild(&ctx.cache)
    .await
    .ok_or_else(|| StringError::from("Only Guilds supported"))?;
  let guild_id = guild.id;
  let queue_lock = get_queue_for_guild(ctx, guild_id, bot_id).await?;
  let mut queue = queue_lock.write().await;
  queue.resume().await
}

#[command]
#[aliases("pause")]
async fn stop(ctx: &Context, msg: &Message) -> CommandResult {
  let ctx_read = ctx.data.read().await;
  let bot_id = ctx_read.get::<crate::BotIdKey>().unwrap().clone();
  drop(ctx_read);

  // set queue status to paused
  // if there is a current song playing, pause it
  let guild = msg
    .guild(&ctx.cache)
    .await
    .ok_or_else(|| StringError::from("Only Guilds supported"))?;
  let guild_id = guild.id;
  let queue_lock = get_queue_for_guild(ctx, guild_id, bot_id).await?;
  let mut queue = queue_lock.write().await;
  queue.pause().await
}

#[command]
async fn skip(ctx: &Context, msg: &Message) -> CommandResult {
  let ctx_read = ctx.data.read().await;
  let bot_id = ctx_read.get::<crate::BotIdKey>().unwrap().clone();
  drop(ctx_read);

  let guild = msg
    .guild(&ctx.cache)
    .await
    .ok_or_else(|| StringError::from("Only Guilds supported"))?;
  let guild_id = guild.id;
  drop(guild);
  let queue_lock = get_queue_for_guild(ctx, guild_id, bot_id).await?;
  let mut queue = queue_lock.write().await;
  queue.next_song().await?;
  queue.state_update();
  Ok(())
}

// 1. Find voice manager for guild -> leave
// 2. Find queue for guild -> set connected_channel to None
#[command]
#[aliases("l")]
async fn leave(ctx: &Context, msg: &Message) -> CommandResult {
  let ctx_read = ctx.data.read().await;
  let bot_id = ctx_read.get::<crate::BotIdKey>().unwrap().clone();
  drop(ctx_read);

  let guild = msg
    .guild(&ctx.cache)
    .await
    .ok_or_else(|| StringError(String::from("Only Guilds supported")))?;
  let voice_manager_lock = ctx
    .data
    .read()
    .await
    .get::<VoiceManager>()
    .cloned()
    .ok_or_else(|| StringError(String::from("Could not find VoiceManager")))?;
  let mut voice_manager = voice_manager_lock.lock().await;
  voice_manager
    .leave(&guild.id)
    .ok_or_else(|| StringError(String::from("Failed to leave channel")))?;
  let queue = get_queue_for_guild(ctx, guild.id, bot_id).await?;
  let mut queue_write_guard = queue.write().await;
  queue_write_guard.connected_channel = None;
  Ok(())
}

async fn get_queue_for_guild(
  ctx: &Context,
  guild_id: GuildId,
  bot_id: UserId,
) -> Result<ThreadSafeQueue, StringError> {
  log::info!("getting write lock for data");
  let mut data_write = ctx.data.write().await;
  log::info!("got it");
  let volume = data_write
    .get::<VolumeMapKey>()
    .map(|m| m.get(&guild_id.0))
    .flatten()
    .cloned();
  log::info!("got volume");
  let mut queue_map = data_write
    .get_mut::<QueueKey>()
    .ok_or_else(|| StringError(String::from("Could not find Queue map in Context")))?
    .write()
    .await;
  log::info!("got queue map");
  let entry = queue_map
    .entry(guild_id)
    .or_insert(Queue::new(bot_id, ctx.cache.clone(), Arc::clone(&ctx.http), volume).await);
  log::info!("got entry");
  Ok(Arc::clone(&entry))
}
