#[derive(Debug, Clone)]
pub struct SearchResult {
  pub url: String,
  pub title: String,
  pub author: String,
}

#[derive(Debug)]
pub enum Site {
  YouTube,
  SoundCloud,
}

impl std::str::FromStr for Site {
  type Err = ();

  fn from_str(string: &str) -> Result<Self, Self::Err> {
    match &*string.to_lowercase() {
      "youtube" | "yt" => Ok(Site::YouTube),
      "soundcloud" | "sc" => Ok(Site::SoundCloud),
      _ => Err(()),
    }
  }
}

pub mod youtube {
  use super::SearchResult;
  use serde::{Deserialize, Serialize};
  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct QueryRoot {
    pub kind: String,
    pub etag: String,
    pub next_page_token: Option<String>,
    pub region_code: String,
    pub page_info: PageInfo,
    pub items: Vec<Item>,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct PageInfo {
    pub total_results: i64,
    pub results_per_page: i64,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct Item {
    pub kind: String,
    pub etag: String,
    pub id: Id,
    pub snippet: Snippet,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct Id {
    pub kind: String,
    pub video_id: String,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct Snippet {
    pub published_at: String,
    pub channel_id: String,
    pub title: String,
    pub description: String,
    pub thumbnails: Thumbnails,
    pub channel_title: String,
    pub live_broadcast_content: String,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct Thumbnails {
    pub default: Default,
    pub medium: Medium,
    pub high: High,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct Default {
    pub url: String,
    pub width: i64,
    pub height: i64,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct Medium {
    pub url: String,
    pub width: i64,
    pub height: i64,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct High {
    pub url: String,
    pub width: i64,
    pub height: i64,
  }

  impl From<Item> for SearchResult {
    fn from(item: Item) -> SearchResult {
      SearchResult {
        url: format!("https://youtube.com/watch?v={}", item.id.video_id),
        title: item.snippet.title,
        author: item.snippet.channel_title,
      }
    }
  }

  pub fn search(
    http_client: &reqwest::blocking::Client,
    query: &str,
    api_key: &str,
  ) -> Result<Vec<SearchResult>, Box<dyn std::error::Error + Send + Sync>> {
    log::info!("Searching youtube with query: {}", query);
    let url_encoded: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    let req = http_client.get(&*format!(
      "https://www.googleapis.com/youtube/v3/search?order=relevance&q={}&type=video&key={}&part=snippet",
      &*url_encoded, api_key,
    ));
    let results = req
      .send()
      .map_err(|e| Box::new(e))?
      .json::<QueryRoot>()
      .map(|root| {
        let items = root.items.len();
        root
          .items
          .into_iter()
          .fold(Vec::with_capacity(items), |mut acc, item| {
            acc.push(SearchResult::from(item));
            acc
          })
      })
      .map_err(|e| Box::new(e))?;
    Ok(results)
  }
}

pub mod soundcloud {
  use super::SearchResult;
  use serde::{Deserialize, Serialize};
  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct QueryRoot {
    pub collection: Vec<Collection>,
    #[serde(rename = "total_results")]
    pub total_results: i64,
    #[serde(rename = "next_href")]
    pub next_href: String,
    #[serde(rename = "query_urn")]
    pub query_urn: String,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct Collection {
    #[serde(rename = "comment_count")]
    pub comment_count: Option<i64>,
    #[serde(rename = "full_duration")]
    pub full_duration: Option<i64>,
    pub downloadable: Option<bool>,
    #[serde(rename = "created_at")]
    pub created_at: String,
    pub description: Option<String>,
    pub media: Option<Media>,
    pub title: Option<String>,
    #[serde(rename = "publisher_metadata")]
    pub publisher_metadata: Option<PublisherMetadata>,
    pub duration: Option<i64>,
    #[serde(rename = "has_downloads_left")]
    pub has_downloads_left: Option<bool>,
    #[serde(rename = "artwork_url")]
    pub artwork_url: Option<String>,
    pub public: Option<bool>,
    pub streamable: Option<bool>,
    #[serde(rename = "tag_list")]
    pub tag_list: Option<String>,
    pub genre: Option<String>,
    pub id: i64,
    #[serde(rename = "reposts_count")]
    pub reposts_count: Option<i64>,
    pub state: Option<String>,
    #[serde(rename = "label_name")]
    pub label_name: Option<::serde_json::Value>,
    #[serde(rename = "last_modified")]
    pub last_modified: String,
    pub commentable: Option<bool>,
    pub policy: Option<String>,
    pub visuals: Option<::serde_json::Value>,
    pub kind: Option<String>,
    #[serde(rename = "purchase_url")]
    pub purchase_url: Option<String>,
    pub sharing: Option<String>,
    pub uri: String,
    #[serde(rename = "secret_token")]
    pub secret_token: Option<::serde_json::Value>,
    #[serde(rename = "download_count")]
    pub download_count: Option<i64>,
    #[serde(rename = "likes_count")]
    pub likes_count: Option<i64>,
    pub urn: String,
    pub license: Option<String>,
    #[serde(rename = "purchase_title")]
    pub purchase_title: Option<String>,
    #[serde(rename = "display_date")]
    pub display_date: Option<String>,
    #[serde(rename = "embeddable_by")]
    pub embeddable_by: Option<String>,
    #[serde(rename = "release_date")]
    pub release_date: Option<::serde_json::Value>,
    #[serde(rename = "user_id")]
    pub user_id: Option<i64>,
    #[serde(rename = "monetization_model")]
    pub monetization_model: Option<String>,
    #[serde(rename = "waveform_url")]
    pub waveform_url: Option<String>,
    pub permalink: String,
    #[serde(rename = "permalink_url")]
    pub permalink_url: String,
    pub user: Option<User>,
    #[serde(rename = "playback_count")]
    pub playback_count: Option<i64>,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct Media {
    pub transcodings: Vec<Transcoding>,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct Transcoding {
    pub url: String,
    pub preset: String,
    pub duration: Option<i64>,
    pub snipped: bool,
    pub format: Format,
    pub quality: String,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct Format {
    pub protocol: String,
    #[serde(rename = "mime_type")]
    pub mime_type: String,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct PublisherMetadata {
    pub urn: String,
    #[serde(rename = "contains_music")]
    pub contains_music: Option<bool>,
    pub artist: Option<String>,
    pub isrc: Option<String>,
    pub id: i64,
    #[serde(rename = "album_title")]
    pub album_title: Option<String>,
    #[serde(rename = "p_line_for_display")]
    pub p_line_for_display: Option<String>,
    #[serde(rename = "writer_composer")]
    pub writer_composer: Option<String>,
    pub iswc: Option<String>,
    pub publisher: Option<String>,
    #[serde(rename = "upc_or_ean")]
    pub upc_or_ean: Option<String>,
    #[serde(rename = "p_line")]
    pub p_line: Option<String>,
    #[serde(rename = "release_title")]
    pub release_title: Option<String>,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct User {
    #[serde(rename = "avatar_url")]
    pub avatar_url: String,
    pub city: Option<String>,
    #[serde(rename = "comments_count")]
    pub comments_count: i64,
    #[serde(rename = "country_code")]
    pub country_code: Option<String>,
    #[serde(rename = "created_at")]
    pub created_at: String,
    #[serde(rename = "creator_subscriptions")]
    pub creator_subscriptions: Vec<CreatorSubscription>,
    #[serde(rename = "creator_subscription")]
    pub creator_subscription: CreatorSubscription2,
    pub description: Option<String>,
    #[serde(rename = "followers_count")]
    pub followers_count: i64,
    #[serde(rename = "followings_count")]
    pub followings_count: i64,
    #[serde(rename = "first_name")]
    pub first_name: String,
    #[serde(rename = "full_name")]
    pub full_name: String,
    #[serde(rename = "groups_count")]
    pub groups_count: i64,
    pub id: i64,
    pub kind: String,
    #[serde(rename = "last_modified")]
    pub last_modified: String,
    #[serde(rename = "last_name")]
    pub last_name: String,
    #[serde(rename = "likes_count")]
    pub likes_count: i64,
    #[serde(rename = "playlist_likes_count")]
    pub playlist_likes_count: i64,
    pub permalink: String,
    #[serde(rename = "permalink_url")]
    pub permalink_url: String,
    #[serde(rename = "playlist_count")]
    pub playlist_count: i64,
    #[serde(rename = "reposts_count")]
    pub reposts_count: ::serde_json::Value,
    #[serde(rename = "track_count")]
    pub track_count: i64,
    pub uri: String,
    pub urn: String,
    pub username: String,
    pub verified: bool,
    pub visuals: Option<Visuals>,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct CreatorSubscription {
    pub product: Product,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct Product {
    pub id: String,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct CreatorSubscription2 {
    pub product: Product2,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct Product2 {
    pub id: String,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct Visuals {
    pub urn: String,
    pub enabled: bool,
    pub visuals: Vec<Visual>,
    pub tracking: Option<::serde_json::Value>,
  }

  #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  pub struct Visual {
    pub urn: String,
    #[serde(rename = "entry_time")]
    pub entry_time: i64,
    #[serde(rename = "visual_url")]
    pub visual_url: String,
  }

  impl From<Collection> for SearchResult {
    fn from(collection: Collection) -> SearchResult {
      SearchResult {
        title: collection.title.unwrap_or(collection.uri),
        author: collection
          .user
          .map(|u| u.username)
          .unwrap_or_else(|| String::from("Unknown User")),
        url: collection.permalink_url,
      }
    }
  }

  pub fn search(
    http_client: &reqwest::blocking::Client,
    query: &str,
    client_id: &str,
  ) -> Result<Vec<SearchResult>, Box<dyn std::error::Error + Send + Sync>> {
    let url_encoded: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    let req = http_client.get(&*format!(
      "https://api-v2.soundcloud.com/search?q={}&client_id={}",
      &*url_encoded, client_id,
    ));
    let results = req
      .send()
      .map_err(|e| Box::new(e))?
      .json::<QueryRoot>()
      .map(|root| {
        let items = root.collection.len();
        root
          .collection
          .into_iter()
          .fold(Vec::with_capacity(items), |mut acc, item| {
            acc.push(SearchResult::from(item));
            acc
          })
      })
      .map_err(|e| Box::new(e))?;
    Ok(results)
  }
}
