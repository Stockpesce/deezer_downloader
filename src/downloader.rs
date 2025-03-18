use block_modes::{block_padding::NoPadding, BlockMode, BlockModeError, Cbc};
use blowfish::Blowfish;
use reqwest::{Client, ClientBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub use reqwest::Proxy;

use crate::error::DeezerApiError;

const PRIVATE_DEEZER_API_LINK: &str = "https://www.deezer.com/ajax/gw-light.php";
const PRIVATE_DEEZER_MEDIA_LINK: &str = "https://media.deezer.com/v1/get_url";
const SECRET_KEY: &[u8; 16] = b"g4el58wc0zvf9na1";
const SECRET_IV: [u8; 8] = hex_literal::hex!("0001020304050607");

#[derive(Debug, Serialize)]
#[serde(tag = "method")]
pub enum DeezerApiRequest {
    #[serde(rename = "deezer.getUserData")]
    UserData,

    #[serde(rename = "song.getData")]
    SongData {
        #[serde(rename = "sng_id")]
        id: u64,
    },
}

#[derive(Debug, Deserialize)]
pub struct DeezerApiResponse {
    pub error: Value,
    pub results: Value,
}

pub struct DownloaderBuilder {
    client_builder: reqwest::ClientBuilder,
    token: Option<String>,
}

impl DownloaderBuilder {
    pub fn new() -> Self {
        let client_builder = reqwest::ClientBuilder::new().cookie_store(true);

        Self {
            client_builder,
            token: None,
        }
    }

    pub fn token(mut self, token: String) -> Self {
        self.token = Some(token);
        self
    }

    pub fn accept_invalid_certs(mut self, accept: bool) -> Self {
        self.client_builder = self.client_builder.danger_accept_invalid_certs(accept);
        self
    }

    pub fn proxy(mut self, proxy: reqwest::Proxy) -> Self {
        self.client_builder = self.client_builder.proxy(proxy);
        self
    }

    pub async fn build(self) -> anyhow::Result<Downloader> {
        let mut downloader = Downloader {
            client: self.client_builder.build()?,
            token: None,
            license_token: None,
        };

        downloader.update_tokens().await?;
        log::info!("Created downloader");
        Ok(downloader)
    }
}

/// public interface for downloading songs
pub struct Downloader {
    client: Client,
    license_token: Option<String>,
    token: Option<String>,
}

impl Downloader {
    pub fn client(&self) -> &Client {
        &self.client
    }
    async fn api_get(&self, api_request: DeezerApiRequest) -> anyhow::Result<Value> {
        let token = match &self.token {
            Some(token) => token.as_str(),
            None => "null",
        };

        #[derive(Debug, Serialize)]
        struct DeezerApiRequestInternal<'a> {
            api_version: &'a str,
            api_token: &'a str,
            input: &'a str,
            #[serde(flatten)]
            deezer_api_requst: DeezerApiRequest,
        }

        let deezer_api_request = DeezerApiRequestInternal {
            api_version: "1.0",
            api_token: token,
            input: "3",
            deezer_api_requst: api_request,
        };

        let response = self
            .client
            .post(PRIVATE_DEEZER_API_LINK)
            .json(&deezer_api_request)
            .send()
            .await?
            .json::<DeezerApiResponse>()
            .await?;

        if let Value::Object(errors) = response.error {
            Err(anyhow::Error::from(DeezerApiError::from(&errors)))
        } else {
            Ok(response.results)
        }
    }

    /// update user and license tokens. This might be needed in case of [DeezerApiError::InvalidToken]
    pub async fn update_tokens(&mut self) -> anyhow::Result<()> {
        let userdata = self.api_get(DeezerApiRequest::UserData).await?;
        self.token = Some(userdata["checkForm"].as_str().unwrap().to_string());
        let userdata = self.api_get(DeezerApiRequest::UserData).await?;
        self.license_token = Some(
            userdata["USER"]["OPTIONS"]["license_token"]
                .as_str()
                .unwrap()
                .to_string(),
        );

        Ok(())
    }

    pub async fn new() -> anyhow::Result<Self> {
        DownloaderBuilder::new().build().await
    }

    /// this function returns raw data for a song as Vec<u8>

    pub async fn dowload_raw_song_data(&self, id: u64) -> anyhow::Result<Vec<u8>> {
        let data = self.api_get(DeezerApiRequest::SongData { id }).await?;
        let token = if let Value::Object(fallback) = &data["FALLBACK"] {
            fallback.get("TRACK_TOKEN")
        } else {
            data.get("TRACK_TOKEN")
        }
        .unwrap();

        let Some(license_token) = &self.license_token else {
            return Err(anyhow::anyhow!("no license token"));
        };

        let get_song_url_request = json!({
            "license_token": license_token,
            "media": [
                {
                    "type": "FULL",
                    "formats": [
                        {
                            "cipher": "BF_CBC_STRIPE",
                            "format": "MP3_320"
                        },
                        {
                            "cipher": "BF_CBC_STRIPE",
                            "format": "MP3_128"
                        },
                        {
                            "cipher": "BF_CBC_STRIPE",
                            "format": "MP3_64"
                        },
                        {
                            "cipher": "BF_CBC_STRIPE",
                            "format": "MP3_MISC"
                        }
                    ]
                }
            ],
            "track_tokens": [
                token
            ]
        });

        let get_song_url_response = self
            .client
            .post(PRIVATE_DEEZER_MEDIA_LINK)
            .json(&get_song_url_request)
            .send()
            .await?
            .json::<Value>()
            .await?;

        let data = &get_song_url_response["data"][0]["media"][0];
        let song_url = data["sources"][0]["url"].as_str().unwrap();

        let encrypted_song = self
            .client
            .get(song_url)
            .send()
            .await?
            .bytes()
            .await?
            .to_vec();

        let hash = md5::compute(id.to_string()).0;
        let hash = hex::encode(hash).as_bytes().to_vec();
        let key = (0..16).fold("".to_string(), |acc, i| {
            let byte = (hash[i] ^ hash[i + 16] ^ SECRET_KEY[i]) as char;
            let mut acc = acc;
            acc.push(byte);
            acc
        });

        let decrypted_song: Result<Vec<Vec<u8>>, BlockModeError> = encrypted_song
            .chunks(2048)
            .enumerate()
            .map(|(index, chunk)| {
                if index % 3 == 0 && chunk.len() == 2048 {
                    let blowfish: Cbc<Blowfish, NoPadding> =
                        Cbc::new_from_slices(key.as_bytes(), &SECRET_IV).unwrap();

                    blowfish.decrypt_vec(chunk)
                } else {
                    Ok(chunk.to_vec())
                }
            })
            .collect();

        match decrypted_song {
            Ok(song) => Ok(song.into_iter().flatten().collect()),
            Err(err) => Err(anyhow::Error::new(DeezerApiError::from(err))),
        }
    }
}
