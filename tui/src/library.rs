//! A minimal Subsonic API client for browsing — the same endpoints and auth
//! scheme `maraetai-service`'s own web client uses (`internal/web/static/
//! api.js` in that repo), so response shapes are known-good rather than
//! guessed: `getAlbumList2` for the album list, `getAlbum` for an album's
//! songs, `search3` for search.

use anyhow::{Context, Result, bail};
use maraetai_common::Credentials;
use maraetai_common::auth::AuthParams;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct Album {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub artist: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Artist {
    pub id: String,
    pub name: String,
    #[serde(default, rename = "albumCount")]
    pub album_count: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    #[serde(default, rename = "songCount")]
    pub song_count: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Genre {
    pub value: String,
    #[serde(default, rename = "songCount")]
    pub song_count: u32,
    #[serde(default, rename = "albumCount")]
    pub album_count: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Song {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    /// Seconds. Subsonic sends an integer; `f64` accepts that fine and lets
    /// the daemon's `PlayUrl` (which wants seconds as `f64`) take it as-is.
    #[serde(default)]
    pub duration: f64,
    #[serde(default, rename = "coverArt")]
    pub cover_art: Option<String>,
}

pub struct Client {
    creds: Credentials,
    http: reqwest::Client,
}

impl Client {
    pub fn new(creds: Credentials) -> Self {
        Self {
            creds,
            http: reqwest::Client::new(),
        }
    }

    /// Builds an authenticated, freshly-salted `/rest/stream.view` URL for a
    /// song id — used both by browsing (play a selected song) and by the
    /// `maraetai play <song-id>` CLI command.
    pub fn stream_url(&self, song_id: &str) -> String {
        self.authed_url("rest/stream.view", &[("id", song_id)])
    }

    /// Builds an authenticated `/rest/getCoverArt.view` URL, if `cover_art`
    /// (the opaque id Subsonic gives each song/album) is present. Feeds
    /// MPRIS's `art_url` metadata field — desktop notification widgets and
    /// media-key overlays that show cover art read this.
    pub fn cover_art_url(&self, cover_art: &Option<String>) -> String {
        match cover_art {
            Some(id) => self.authed_url("rest/getCoverArt.view", &[("id", id)]),
            None => String::new(),
        }
    }

    pub async fn albums(&self) -> Result<Vec<Album>> {
        let mut root = self
            .get_json("rest/getAlbumList2.view", &[("type", "alphabeticalByName"), ("size", "200")])
            .await?;
        parse(root["albumList2"]["album"].take())
    }

    pub async fn album_songs(&self, album_id: &str) -> Result<Vec<Song>> {
        let mut root = self.get_json("rest/getAlbum.view", &[("id", album_id)]).await?;
        parse(root["album"]["song"].take())
    }

    pub async fn search(&self, query: &str) -> Result<Vec<Song>> {
        let mut root = self
            .get_json(
                "rest/search3.view",
                &[("query", query), ("artistCount", "0"), ("albumCount", "0"), ("songCount", "50")],
            )
            .await?;
        parse(root["searchResult3"]["song"].take())
    }

    /// The full artist index, flattened across Subsonic's alphabetical index
    /// groups (`artists.index[].artist[]`) — the same flattening the web
    /// client does client-side.
    pub async fn artists(&self) -> Result<Vec<Artist>> {
        let root = self.get_json("rest/getArtists.view", &[]).await?;
        let groups = root["artists"]["index"].as_array().cloned().unwrap_or_default();
        let mut out = Vec::new();
        for mut group in groups {
            out.extend(parse::<Artist>(group["artist"].take())?);
        }
        Ok(out)
    }

    pub async fn artist_albums(&self, artist_id: &str) -> Result<Vec<Album>> {
        let mut root = self.get_json("rest/getArtist.view", &[("id", artist_id)]).await?;
        parse(root["artist"]["album"].take())
    }

    pub async fn playlists(&self) -> Result<Vec<Playlist>> {
        let mut root = self.get_json("rest/getPlaylists.view", &[]).await?;
        parse(root["playlists"]["playlist"].take())
    }

    /// Note: Subsonic's `getPlaylist` nests its songs under `entry`, not
    /// `song` (unlike every other endpoint here) — matched exactly against
    /// `maraetai-service`'s web client, which has the same quirk.
    pub async fn playlist_songs(&self, playlist_id: &str) -> Result<Vec<Song>> {
        let mut root = self.get_json("rest/getPlaylist.view", &[("id", playlist_id)]).await?;
        parse(root["playlist"]["entry"].take())
    }

    pub async fn genres(&self) -> Result<Vec<Genre>> {
        let mut root = self.get_json("rest/getGenres.view", &[]).await?;
        parse(root["genres"]["genre"].take())
    }

    pub async fn albums_by_genre(&self, genre: &str) -> Result<Vec<Album>> {
        let mut root = self
            .get_json("rest/getAlbumList2.view", &[("type", "byGenre"), ("genre", genre), ("size", "200")])
            .await?;
        parse(root["albumList2"]["album"].take())
    }

    fn authed_url(&self, path: &str, extra: &[(&str, &str)]) -> String {
        let auth = AuthParams::new(&self.creds.username, &self.creds.password);
        let mut pairs: Vec<(String, String)> =
            extra.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        auth.append_to(&mut pairs);
        pairs.push(("f".to_string(), "json".to_string()));
        let query = pairs
            .iter()
            .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{}/{path}?{query}", self.creds.server_url.trim_end_matches('/'))
    }

    async fn get_json(&self, path: &str, extra: &[(&str, &str)]) -> Result<Value> {
        let url = self.authed_url(path, extra);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting {path}"))?
            .error_for_status()
            .with_context(|| format!("{path} returned an error status"))?;
        let mut body: Value = resp.json().await.with_context(|| format!("parsing {path} response"))?;
        let root = body["subsonic-response"].take();
        if root["status"] != "ok" {
            let msg = root["error"]["message"].as_str().unwrap_or("unknown error");
            bail!("{path}: {msg}");
        }
        Ok(root)
    }
}

/// Parses a JSON array value into `Vec<T>`, treating "field absent" (a
/// missing array — e.g. an album with no songs, a query with no results) the
/// same as "empty", rather than an error.
fn parse<T: for<'de> Deserialize<'de>>(value: Value) -> Result<Vec<T>> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    serde_json::from_value(value).context("unexpected response shape")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Starts a one-shot async HTTP server that always returns `body` as a
    /// 200 JSON response, then returns its base URL. Used to check that
    /// `Client`'s JSON-path navigation (`root["albumList2"]["album"]` etc.)
    /// actually matches real Subsonic response shapes, not just that it
    /// compiles.
    async fn respond_once(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        format!("http://{addr}")
    }

    fn test_creds(server_url: String) -> Credentials {
        Credentials {
            server_url,
            username: "alice".into(),
            password: "hunter2".into(),
        }
    }

    #[tokio::test]
    async fn parses_real_shaped_album_list() {
        // Field names/nesting match the Subsonic getAlbumList2 shape that
        // maraetai-service's own web client (internal/web/static/api.js)
        // already relies on.
        let url = respond_once(
            r#"{"subsonic-response":{"status":"ok","albumList2":{"album":[
                {"id":"al1","name":"Mezzanine","artist":"Massive Attack","coverArt":"ar-al1","songCount":11},
                {"id":"al2","name":"Dummy","artist":"Portishead","coverArt":"ar-al2","songCount":11}
            ]}}}"#,
        )
        .await;

        let client = Client::new(test_creds(url));
        let albums = client.albums().await.unwrap();
        assert_eq!(albums.len(), 2);
        assert_eq!(albums[0].id, "al1");
        assert_eq!(albums[0].name, "Mezzanine");
        assert_eq!(albums[0].artist, "Massive Attack");
    }

    #[tokio::test]
    async fn parses_real_shaped_album_songs() {
        let url = respond_once(
            r#"{"subsonic-response":{"status":"ok","album":{"id":"al1","name":"Mezzanine","song":[
                {"id":"s1","title":"Angel","artist":"Massive Attack","album":"Mezzanine","duration":379,"coverArt":"al1"},
                {"id":"s2","title":"Teardrop","artist":"Massive Attack","album":"Mezzanine","duration":331}
            ]}}}"#,
        )
        .await;

        let client = Client::new(test_creds(url));
        let songs = client.album_songs("al1").await.unwrap();
        assert_eq!(songs.len(), 2);
        assert_eq!(songs[0].title, "Angel");
        assert_eq!(songs[0].duration, 379.0);
        assert_eq!(songs[0].cover_art.as_deref(), Some("al1"));
        // Missing coverArt on the second song must not be a parse error.
        assert_eq!(songs[1].cover_art, None);
    }

    #[tokio::test]
    async fn empty_search_results_are_not_an_error() {
        let url = respond_once(
            r#"{"subsonic-response":{"status":"ok","searchResult3":{}}}"#,
        )
        .await;

        let client = Client::new(test_creds(url));
        let songs = client.search("nonexistent").await.unwrap();
        assert!(songs.is_empty());
    }

    #[tokio::test]
    async fn subsonic_error_status_surfaces_the_message() {
        let url = respond_once(
            r#"{"subsonic-response":{"status":"failed","error":{"code":40,"message":"Wrong username or password"}}}"#,
        )
        .await;

        let client = Client::new(test_creds(url));
        let err = client.albums().await.unwrap_err();
        assert!(err.to_string().contains("Wrong username or password"));
    }

    #[test]
    fn cover_art_url_is_empty_when_absent() {
        let client = Client::new(test_creds("https://example.com".into()));
        assert_eq!(client.cover_art_url(&None), "");
        assert!(client.cover_art_url(&Some("art1".into())).contains("id=art1"));
    }
}
