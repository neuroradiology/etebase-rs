extern crate serde_json;
extern crate hex;

use url::Url;

use serde::{Serialize, Deserialize};

use reqwest::{
    blocking::Client,
    header,
};

use super::{
    crypto::{
        memcmp,
        CryptoManager,
        AsymmetricKeyPair,
        AsymmetricCryptoManager,
    },
    content::{
        CollectionInfo,
        SyncEntry,
    },
    error::{
        Result,
        Error,
    }
};

static APP_USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
);

pub const SERVICE_API_URL: &str = "https://api.etesync.com";

const HMAC_SIZE: usize = 32;

pub fn get_client(client_name: &str) -> Result<Client> {
    let client = Client::builder()
        .user_agent(format!("{} {}", client_name, APP_USER_AGENT))
        .build()?;

    Ok(client)
}

pub fn test_reset(client: &Client, auth_token: &str, api_base: &str) -> Result<()> {
    let api_base = Url::parse(api_base)?;
    let url = api_base.join("reset/")?;

    let res = client.post(url.as_str())
        .header(header::AUTHORIZATION, format!("Token {}", auth_token))
        .send()?;

    res.error_for_status()?;

    Ok(())
}

pub struct Authenticator<'a> {
    api_base: Url,
    client: &'a Client,
}

impl Authenticator<'_> {
    pub fn new<'a>(client: &'a Client, api_base: &str) -> Authenticator<'a> {
        Authenticator {
            api_base: Url::parse(api_base).unwrap(),
            client,
        }
    }

    pub fn get_token(&self, username: &str, password: &str) -> Result<String> {
        let url = self.api_base.join("api-token-auth/")?;
        let params = [("username", username), ("password", password)];
        let res = self.client.post(url.as_str())
            .form(&params)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded;charset=UTF-8")
            .header(header::ACCEPT, "application/json")
            .send()?;

        #[derive(Deserialize)]
        struct TokenResponse {
            token: String,
        }

        let json = res.json::<TokenResponse>()?;

        Ok(json.token)
    }

    pub fn invalidate_token(&self, auth_token: &str) -> Result<String> {
        let url = self.api_base.join("api/logout/")?;
        let res = self.client.post(url.as_str())
            .header(header::AUTHORIZATION, format!("Token {}", auth_token))
            .send()?;

        Ok(res.text()?)
    }
}

fn get_base_headers(auth_token: &str, capacity: usize) -> header::HeaderMap<header::HeaderValue> {
    let capacity = capacity + 3;
    let mut map = header::HeaderMap::with_capacity(capacity);
    map.insert(header::CONTENT_TYPE, "application/json;charset=UTF-8".parse().unwrap());
    map.insert(header::ACCEPT, "application/json".parse().unwrap());
    map.insert(header::AUTHORIZATION, format!("Token {}", auth_token).parse().unwrap());

    map
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JournalJson {
    uid: String,
    version: u8,
    owner: String,
    content: String,
    read_only: Option<bool>,
    key: Option<String>,
    last_uid: Option<String>,
}

#[derive(Clone)]
pub struct Journal {
    pub uid: String,
    pub version: u8,
    pub owner: String,
    pub content: Vec<u8>,
    pub key: Option<Vec<u8>>,

    read_only: bool,
    last_uid: Option<String>,
}

impl Journal {
    pub fn new(uid: &str, version: u8, owner: &str) -> Journal {
        Journal {
            uid: uid.to_owned(),
            version,
            owner: owner.to_owned(),
            content: vec![],
            key: None,

            read_only: false,
            last_uid: None,
        }
    }

    // FIXME: this should return a result
    fn from_json(uid: &str, json: &JournalJson) -> Journal {
        Journal {
            uid: uid.to_owned(),
            version: json.version,
            owner: json.owner.clone(),
            read_only: match json.read_only {
                Some(val) => val,
                None => false,
            },
            content: base64::decode(&json.content).unwrap(),
            key: match &json.key {
                Some(val) => Some(base64::decode(val).unwrap()),
                None => None,
            },
            last_uid: json.last_uid.clone(),
        }
    }

    fn to_json(&self) -> JournalJson {
        JournalJson {
            uid: self.uid.clone(),
            version: self.version,
            owner: self.owner.clone(),
            read_only: Some(self.read_only),
            content: base64::encode(&self.content),
            key: match &self.key {
                Some(val) => Some(base64::encode(val)),
                None => None,
            },
            last_uid: None,
        }
    }

    pub fn is_read_only(&self) -> bool {
        return self.read_only;
    }

    pub fn get_last_uid(&self) -> &Option<String> {
        return &self.last_uid;
    }

    pub fn get_crypto_manager(&self, key: &[u8], keypair: &AsymmetricKeyPair) -> Result<CryptoManager> {
        if let Some(key) = &self.key {
            let asymmetric_crypto_manager = AsymmetricCryptoManager::new(&keypair);
            let derived = asymmetric_crypto_manager.decrypt(&key)?;

            return CryptoManager::from_derived_key(&derived, self.version);
        } else {
            return CryptoManager::new(key, &self.uid, self.version);
        }
    }

    pub fn set_info(&mut self, crypto_manager: &CryptoManager, info: &CollectionInfo) -> Result<()> {
        let json = serde_json::to_vec(&info)?;
        let ciphertext = crypto_manager.encrypt(&json)?;
        let hmac = Journal::calculate_hmac(crypto_manager, &ciphertext, &self.uid.as_bytes())?;
        let mut content = hmac;
        content.extend(ciphertext);
        self.content = content;

        Ok(())
    }

    pub fn get_info(&self, crypto_manager: &CryptoManager) -> Result<CollectionInfo> {
        self.verify(&crypto_manager)?;

        let ciphertext = &self.content[HMAC_SIZE..];
        let info = crypto_manager.decrypt(ciphertext)?;
        let info = serde_json::from_slice(&info)?;

        Ok(info)
    }

    fn calculate_hmac(crypto_manager: &CryptoManager, message: &[u8], uid: &[u8]) -> Result<Vec<u8>> {
        let mut data = uid.to_vec();
        data.extend(message);
        let hmac = crypto_manager.hmac(&data)?;

        Ok(hmac)
    }

    fn verify(&self, crypto_manager: &CryptoManager) -> Result<()> {
        let hmac = &self.content[..HMAC_SIZE];
        let ciphertext = &self.content[HMAC_SIZE..];
        let calculated = Journal::calculate_hmac(crypto_manager, &ciphertext, &self.uid.as_bytes())?;

        if memcmp(&hmac, &calculated) {
            return Ok(());
        } else {
            return Err(Error::from("HMAC mismatch"));
        }
    }
}

pub struct JournalManager {
    api_base: Url,
    client: Client,
    auth_token: String,
}

impl JournalManager {
    pub fn new(client: &Client, auth_token: &str, api_base: &str) -> JournalManager {
        let api_base = Url::parse(api_base).unwrap();
        let api_base = api_base.join("api/v1/journals/").unwrap();
        JournalManager {
            api_base,
            client: client.clone(),
            auth_token: auth_token.to_owned(),
        }
    }

    pub fn fetch(&self, journal_uid: &str) -> Result<Journal> {
        let url = self.api_base.join(&format!{"{}/", journal_uid})?;
        let headers = get_base_headers(&self.auth_token, 0);
        let res = self.client.get(url.as_str())
            .headers(headers)
            .send()?;

        let res = res.error_for_status()?;

        let journal_json = res.json::<JournalJson>()?;
        Ok(Journal::from_json(journal_uid, &journal_json))
    }

    pub fn list(&self) -> Result<Vec<Journal>> {
        let url = &self.api_base;
        let headers = get_base_headers(&self.auth_token, 0);
        let res = self.client.get(url.as_str())
            .headers(headers)
            .send()?;

        let res = res.error_for_status()?;

        let journals_json = res.json::<Vec<JournalJson>>()?;
        Ok(journals_json.into_iter().map(|journal_json| Journal::from_json(&journal_json.uid, &journal_json)).collect())
    }

    pub fn create(&self, journal: &Journal) -> Result<()> {
        let url = &self.api_base;
        let headers = get_base_headers(&self.auth_token, 0);

        let journal_json = journal.to_json();

        let res = self.client.post(url.as_str())
            .headers(headers)
            .json(&journal_json)
            .send()?;

        res.error_for_status()?;

        Ok(())
    }

    pub fn update(&self, journal: &Journal) -> Result<()> {
        let url = self.api_base.join(&format!{"{}/", &journal.uid})?;
        let headers = get_base_headers(&self.auth_token, 0);

        let journal_json = journal.to_json();

        let res = self.client.put(url.as_str())
            .headers(headers)
            .json(&journal_json)
            .send()?;

        res.error_for_status()?;

        Ok(())
    }

    pub fn delete(&self, journal: &Journal) -> Result<()> {
        let url = self.api_base.join(&format!{"{}/", &journal.uid})?;
        let headers = get_base_headers(&self.auth_token, 0);

        let res = self.client.delete(url.as_str())
            .headers(headers)
            .send()?;

        res.error_for_status()?;

        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[allow(non_snake_case)]
struct EntryJson {
    uid: String,
    content: String,
}

#[derive(Clone)]
pub struct Entry {
    pub uid: String,
    pub content: Vec<u8>,
}

impl Entry {
    pub fn from_sync_entry(crypto_manager: &CryptoManager, sync_entry: &SyncEntry, prev_uid: Option<& str>) -> Result<Entry> {
        let json = serde_json::to_vec(&sync_entry)?;
        let ciphertext = crypto_manager.encrypt(&json)?;
        let hmac = Entry::calculate_hmac(crypto_manager, &ciphertext, prev_uid)?;

        let ret = Entry {
            uid: hex::encode(hmac),
            content: ciphertext,
        };

        Ok(ret)
    }

    // FIXME: this should return a result
    fn from_json(uid: &str, json: &EntryJson) -> Entry {
        #[allow(non_snake_case)]
        Entry {
            uid: uid.to_owned(),
            content: base64::decode(&json.content).unwrap(),
        }
    }

    fn to_json(&self) -> EntryJson {
        EntryJson {
            uid: self.uid.clone(),
            content: base64::encode(&self.content),
        }
    }

    pub fn get_sync_entry(&self, crypto_manager: &CryptoManager, prev_uid: Option<& str>) -> Result<SyncEntry> {
        self.verify(&crypto_manager, prev_uid)?;

        let ciphertext = &self.content;
        let info = crypto_manager.decrypt(ciphertext)?;
        let info = serde_json::from_slice(&info)?;

        Ok(info)
    }

    fn calculate_hmac(crypto_manager: &CryptoManager, message: &[u8], prev_uid: Option<& str>) -> Result<Vec<u8>> {
        let mut data = match prev_uid {
            Some(prev_uid) => prev_uid.as_bytes().to_vec(),
            None => vec![],
        };
        data.extend(message);
        let hmac = crypto_manager.hmac(&data)?;

        Ok(hmac)
    }

    fn verify(&self, crypto_manager: &CryptoManager, prev_uid: Option<& str>) -> Result<()> {
        let calculated = Entry::calculate_hmac(crypto_manager, &self.content, prev_uid)?;
        let hmac = match hex::decode(&self.uid) {
            Ok(hmac) => hmac,
            Err(_e) => return Err(Error::from("Failed decoding uid")),
        };

        if memcmp(&hmac, &calculated) {
            return Ok(());
        } else {
            return Err(Error::from("HMAC mismatch"));
        }
    }
}

pub struct EntryManager {
    api_base: Url,
    client: Client,
    auth_token: String,
}

impl EntryManager {
    pub fn new(client: &Client, auth_token: &str, journal_uid: &str, api_base: &str) -> EntryManager {
        let api_base = Url::parse(api_base).unwrap();
        let api_base = api_base.join(&format!("api/v1/journals/{}/entries/", &journal_uid)).unwrap();
        EntryManager {
            api_base,
            client: client.clone(),
            auth_token: auth_token.to_owned(),
        }
    }

    pub fn list(&self, last_uid: Option<&str>, limit: Option<usize>) -> Result<Vec<Entry>> {
        let mut url = self.api_base.clone();

        if let Some(last_uid) = last_uid {
            let mut query = url.query_pairs_mut();
            query.append_pair("last", &last_uid);
        }
        if let Some(limit) = limit {
            let mut query = url.query_pairs_mut();
            query.append_pair("limit", &limit.to_string());
        }

        let headers = get_base_headers(&self.auth_token, 0);
        let res = self.client.get(url.as_str())
            .headers(headers)
            .send()?;

        let res = res.error_for_status()?;

        let entries_json = res.json::<Vec<EntryJson>>()?;
        Ok(entries_json.into_iter().map(|entry_json| Entry::from_json(&entry_json.uid, &entry_json)).collect())
    }

    pub fn create(&self, entries: &[&Entry], last_uid: Option<&str>) -> Result<()> {
        let mut url = self.api_base.clone();

        if let Some(last_uid) = last_uid {
            let mut query = url.query_pairs_mut();
            query.append_pair("last", &last_uid);
        }
        let headers = get_base_headers(&self.auth_token, 0);

        let entries_json: Vec<EntryJson> = entries.into_iter().map(|entry| entry.to_json()).collect();

        let res = self.client.post(url.as_str())
            .headers(headers)
            .json(&entries_json)
            .send()?;

        res.error_for_status()?;

        Ok(())
    }
}


#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserInfoJson {
    owner: Option<String>,
    version: u8,
    pubkey: String,
    content: Option<String>,
}

#[derive(Clone)]
pub struct UserInfo {
    pub owner: Option<String>,
    pub version: u8,
    pub pubkey: Vec<u8>,
    pub content: Option<Vec<u8>>,
}

impl UserInfo {
    pub fn new(owner: &str, version: u8) -> UserInfo {
        UserInfo {
            owner: Some(owner.to_owned()),
            version,
            pubkey: vec![],
            content: None,
        }
    }

    // FIXME: this should return a result
    fn from_json(owner: &str, json: &UserInfoJson) -> UserInfo {
        UserInfo {
            owner: Some(owner.to_owned()),
            version: json.version,
            pubkey: base64::decode(&json.pubkey).unwrap(),
            content: match &json.content {
                Some(val) => Some(base64::decode(val).unwrap()),
                None => None,
            },
        }
    }

    fn to_json(&self) -> UserInfoJson {
        UserInfoJson {
            owner: self.owner.clone(),
            version: self.version,
            pubkey: base64::encode(&self.pubkey),
            content: match &self.content {
                Some(val) => Some(base64::encode(val)),
                None => None,
            },
        }
    }

    pub fn get_crypto_manager(&self, key: &[u8]) -> Result<CryptoManager> {
        return CryptoManager::new(key, "userInfo", self.version);
    }

    pub fn set_keypair(&mut self, crypto_manager: &CryptoManager, keypair: &AsymmetricKeyPair) -> Result<()> {
        let ciphertext = crypto_manager.encrypt(&keypair.get_skey()?)?;
        self.pubkey = keypair.get_pkey()?;
        let hmac = UserInfo::calculate_hmac(crypto_manager, &ciphertext, &self.pubkey)?;
        let mut content = hmac;
        content.extend(ciphertext);
        self.content = Some(content);

        Ok(())
    }

    pub fn get_keypair(&self, crypto_manager: &CryptoManager) -> Result<AsymmetricKeyPair> {
        let content = match &self.content {
            Some(content) => content,
            None => return Err(Error::from("Can't get keypair for someone else's user info")),
        };

        self.verify(&crypto_manager)?;

        let ciphertext = &content[HMAC_SIZE..];
        let skey = crypto_manager.decrypt(ciphertext)?;
        let keypair = AsymmetricKeyPair::from_der(&skey, &self.pubkey)?;

        Ok(keypair)
    }

    fn calculate_hmac(crypto_manager: &CryptoManager, message: &[u8], pubkey: &[u8]) -> Result<Vec<u8>> {
        let mut data = message.to_vec();
        data.extend(pubkey);
        let hmac = crypto_manager.hmac(&data)?;

        Ok(hmac)
    }

    fn verify(&self, crypto_manager: &CryptoManager) -> Result<()> {
        let content = match &self.content {
            Some(content) => content,
            None => return Err(Error::from("Can't verify someone else's user info")),
        };

        let hmac = &content[..HMAC_SIZE];
        let ciphertext = &content[HMAC_SIZE..];
        let calculated = UserInfo::calculate_hmac(crypto_manager, &ciphertext, &self.pubkey)?;

        if memcmp(&hmac, &calculated) {
            Ok(())
        } else {
            Err(Error::from("HMAC mismatch"))
        }
    }
}

pub struct UserInfoManager {
    api_base: Url,
    client: Client,
    auth_token: String,
}

impl UserInfoManager {
    pub fn new(client: &Client, auth_token: &str, api_base: &str) -> UserInfoManager {
        let api_base = Url::parse(api_base).unwrap();
        let api_base = api_base.join("api/v1/user/").unwrap();
        UserInfoManager {
            api_base,
            client: client.clone(),
            auth_token: auth_token.to_owned(),
        }
    }

    pub fn fetch(&self, owner: &str) -> Result<UserInfo> {
        let url = self.api_base.join(&format!{"{}/", owner})?;
        let headers = get_base_headers(&self.auth_token, 0);
        let res = self.client.get(url.as_str())
            .headers(headers)
            .send()?;

        let res = res.error_for_status()?;

        let user_info_json = res.json::<UserInfoJson>()?;
        Ok(UserInfo::from_json(owner, &user_info_json))
    }

    pub fn create(&self, user_info: &UserInfo) -> Result<()> {
        let url = &self.api_base;
        let headers = get_base_headers(&self.auth_token, 0);

        let user_info_json = user_info.to_json();

        let res = self.client.post(url.as_str())
            .headers(headers)
            .json(&user_info_json)
            .send()?;

        res.error_for_status()?;

        Ok(())
    }

    pub fn update(&self, user_info: &UserInfo) -> Result<()> {
        let owner = match &user_info.owner {
            Some(owner) => owner,
            None => return Err(Error::from("Owner is unset")),
        };

        let url = self.api_base.join(&format!{"{}/", owner})?;
        let headers = get_base_headers(&self.auth_token, 0);

        let user_info_json = user_info.to_json();

        let res = self.client.put(url.as_str())
            .headers(headers)
            .json(&user_info_json)
            .send()?;

        res.error_for_status()?;

        Ok(())
    }

    pub fn delete(&self, user_info: &UserInfo) -> Result<()> {
        let owner = match &user_info.owner {
            Some(owner) => owner,
            None => return Err(Error::from("Owner is unset")),
        };

        let url = self.api_base.join(&format!{"{}/", owner})?;
        let headers = get_base_headers(&self.auth_token, 0);

        let res = self.client.delete(url.as_str())
            .headers(headers)
            .send()?;

        res.error_for_status()?;

        Ok(())
    }
}
