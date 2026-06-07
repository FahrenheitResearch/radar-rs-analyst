//! Public radar data-source helpers.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration as StdDuration, Instant};

use chrono::{Datelike, Duration, NaiveDate, Utc};
use serde::Deserialize;
use thiserror::Error;

pub const LEVEL2_ARCHIVE_BUCKET: &str = "unidata-nexrad-level2";
pub const LEVEL2_CHUNKS_BUCKET: &str = "unidata-nexrad-level2-chunks";
const HTTP_CONNECT_TIMEOUT: StdDuration = StdDuration::from_secs(4);
const HTTP_METADATA_TIMEOUT: StdDuration = StdDuration::from_secs(8);
const HTTP_DOWNLOAD_TIMEOUT: StdDuration = StdDuration::from_secs(45);
const HTTP_USER_AGENT: &str = "radar-rs-analyst/0.1 local-desktop";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RadarDataLevel {
    Level2Archive,
    Level2RealtimeChunks,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataSourceKind {
    LocalFile,
    LocalDirectory,
    PublicLevel2Archive,
    PublicLevel2RealtimeChunks,
    NceiArchive,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourcePriority {
    pub sources: Vec<DataSourceKind>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RadarSite {
    pub level2_id: String,
    pub name: Option<String>,
    pub latitude_deg: Option<f32>,
    pub longitude_deg: Option<f32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct S3Object {
    pub key: String,
    pub size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadedObject {
    pub object: S3Object,
    pub path: PathBuf,
    pub url: String,
    pub cache_hit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LatestObject {
    pub object: S3Object,
    pub cache_hit: bool,
}

#[derive(Debug, Error)]
pub enum DataSourceError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("S3 XML parse failed: {0}")]
    Xml(#[from] quick_xml::DeError),
    #[error("JSON parse failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("no objects found for {bucket}/{prefix}")]
    NoObjects { bucket: String, prefix: String },
}

pub type Result<T> = std::result::Result<T, DataSourceError>;

impl Default for SourcePriority {
    fn default() -> Self {
        Self {
            sources: vec![
                DataSourceKind::LocalFile,
                DataSourceKind::PublicLevel2Archive,
            ],
        }
    }
}

impl RadarSite {
    pub fn new(level2_id: impl Into<String>) -> Self {
        let level2_id = level2_id.into().to_ascii_uppercase();
        Self {
            level2_id,
            name: None,
            latitude_deg: None,
            longitude_deg: None,
        }
    }

    pub fn with_location(
        mut self,
        name: Option<String>,
        latitude_deg: Option<f32>,
        longitude_deg: Option<f32>,
    ) -> Self {
        self.name = name;
        self.latitude_deg = latitude_deg;
        self.longitude_deg = longitude_deg;
        self
    }
}

pub fn fallback_sites() -> Vec<RadarSite> {
    FALLBACK_SITE_IDS
        .iter()
        .map(|id| RadarSite::new(*id))
        .collect()
}

pub fn list_level2_sites_for_date(date: NaiveDate) -> Result<Vec<RadarSite>> {
    let prefix = format!("{:04}/{:02}/{:02}/", date.year(), date.month(), date.day());
    let listing = list_s3(LEVEL2_ARCHIVE_BUCKET, &prefix, Some("/"), None)?;
    let mut sites = listing
        .common_prefixes
        .into_iter()
        .filter_map(|prefix| {
            prefix
                .prefix
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .map(str::to_owned)
        })
        .filter(|site| !site.is_empty())
        .map(RadarSite::new)
        .collect::<Vec<_>>();
    sites.sort_by(|left, right| left.level2_id.cmp(&right.level2_id));
    sites.dedup_by(|left, right| left.level2_id == right.level2_id);
    Ok(sites)
}

pub fn list_recent_level2_sites(days_back: i64) -> Result<Vec<RadarSite>> {
    let today = Utc::now().date_naive();
    for offset in 0..=days_back.max(0) {
        let date = today - Duration::days(offset);
        let sites = list_level2_sites_for_date(date)?;
        if !sites.is_empty() {
            return Ok(sites);
        }
    }
    Ok(fallback_sites())
}

pub fn fetch_weather_gov_radar_sites() -> Result<Vec<RadarSite>> {
    let client = metadata_http_client();
    let text = client
        .get("https://api.weather.gov/radar/stations")
        .send()?
        .error_for_status()?
        .text()?;
    let collection: WeatherGovFeatureCollection = serde_json::from_str(&text)?;
    let mut sites = collection
        .features
        .into_iter()
        .filter_map(|feature| {
            let id = feature.properties.id?;
            let coordinates = feature.geometry?.coordinates;
            if coordinates.len() < 2 {
                return None;
            }
            Some(RadarSite::new(id).with_location(
                feature.properties.name,
                Some(coordinates[1] as f32),
                Some(coordinates[0] as f32),
            ))
        })
        .collect::<Vec<_>>();
    sites.sort_by(|left, right| left.level2_id.cmp(&right.level2_id));
    sites.dedup_by(|left, right| left.level2_id == right.level2_id);
    Ok(sites)
}

pub fn fetch_level2_radar_sites(days_back: i64) -> Result<Vec<RadarSite>> {
    let weather_sites = fetch_weather_gov_radar_sites().unwrap_or_default();
    let weather_by_id = weather_sites
        .into_iter()
        .map(|site| (site.level2_id.clone(), site))
        .collect::<BTreeMap<_, _>>();

    let mut sites = list_recent_level2_sites(days_back).unwrap_or_else(|_| fallback_sites());
    for site in &mut sites {
        if let Some(weather_site) = weather_by_id.get(&site.level2_id) {
            site.name = weather_site.name.clone();
            site.latitude_deg = weather_site.latitude_deg;
            site.longitude_deg = weather_site.longitude_deg;
        }
    }
    sites.sort_by(|left, right| left.level2_id.cmp(&right.level2_id));
    sites.dedup_by(|left, right| left.level2_id == right.level2_id);
    Ok(sites)
}

pub fn latest_level2_object(site: &str, days_back: i64) -> Result<S3Object> {
    let today = Utc::now().date_naive();
    for offset in 0..=days_back.max(0) {
        let date = today - Duration::days(offset);
        let prefix = format!(
            "{:04}/{:02}/{:02}/{}/",
            date.year(),
            date.month(),
            date.day(),
            site.to_ascii_uppercase()
        );
        let mut objects = list_s3(LEVEL2_ARCHIVE_BUCKET, &prefix, None, None)?
            .contents
            .into_iter()
            .filter(|object| object.size > 0 && !object.key.ends_with("_MDM"))
            .collect::<Vec<_>>();
        objects.sort_by(|left, right| left.key.cmp(&right.key));
        if let Some(object) = objects.pop() {
            return Ok(object);
        }
    }
    Err(DataSourceError::NoObjects {
        bucket: LEVEL2_ARCHIVE_BUCKET.to_owned(),
        prefix: site.to_owned(),
    })
}

pub fn latest_level2_object_cached(
    site: &str,
    days_back: i64,
    max_age: StdDuration,
) -> Result<LatestObject> {
    let site = site.to_ascii_uppercase();
    let days_back = days_back.max(0);
    let cache_key = LatestObjectCacheKey {
        site: site.clone(),
        days_back,
    };
    if let Ok(cache) = latest_object_cache().lock()
        && let Some(cached) = cache.get(&cache_key)
        && cached.fetched_at.elapsed() <= max_age
    {
        return Ok(LatestObject {
            object: cached.object.clone(),
            cache_hit: true,
        });
    }

    let object = latest_level2_object(&site, days_back)?;
    if let Ok(mut cache) = latest_object_cache().lock() {
        cache.insert(
            cache_key,
            CachedLatestObject {
                object: object.clone(),
                fetched_at: Instant::now(),
            },
        );
    }
    Ok(LatestObject {
        object,
        cache_hit: false,
    })
}

pub fn download_object(
    bucket: &str,
    object: S3Object,
    cache_dir: &Path,
) -> Result<DownloadedObject> {
    fs::create_dir_all(cache_dir)?;
    let filename = object.key.rsplit('/').next().unwrap_or(&object.key);
    let path = cache_dir.join(filename);
    let url = format!("https://{bucket}.s3.amazonaws.com/{}", object.key);
    if path
        .metadata()
        .map(|metadata| metadata.len() == object.size)
        .unwrap_or(false)
    {
        return Ok(DownloadedObject {
            object,
            path,
            url,
            cache_hit: true,
        });
    }

    let bytes = download_http_client()
        .get(&url)
        .send()?
        .error_for_status()?
        .bytes()?;
    let temp_path = path.with_extension("download");
    fs::write(&temp_path, bytes)?;
    fs::rename(&temp_path, &path)?;
    Ok(DownloadedObject {
        object,
        path,
        url,
        cache_hit: false,
    })
}

fn list_s3(
    bucket: &str,
    prefix: &str,
    delimiter: Option<&str>,
    continuation_token: Option<&str>,
) -> Result<S3Listing> {
    let url = format!("https://{bucket}.s3.amazonaws.com/");
    let client = metadata_http_client();
    let mut query = vec![("list-type", "2"), ("prefix", prefix)];
    if let Some(delimiter) = delimiter {
        query.push(("delimiter", delimiter));
    }
    if let Some(token) = continuation_token {
        query.push(("continuation-token", token));
    }
    let text = client
        .get(url)
        .query(&query)
        .send()?
        .error_for_status()?
        .text()?;
    let parsed: S3ListingXml = quick_xml::de::from_str(&text)?;
    Ok(parsed.into())
}

fn metadata_http_client() -> reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            build_http_client(HTTP_METADATA_TIMEOUT)
                .expect("metadata HTTP client should be constructible")
        })
        .clone()
}

fn download_http_client() -> reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            build_http_client(HTTP_DOWNLOAD_TIMEOUT)
                .expect("download HTTP client should be constructible")
        })
        .clone()
}

fn build_http_client(timeout: StdDuration) -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(HTTP_USER_AGENT)
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(timeout)
        .build()?)
}

fn latest_object_cache() -> &'static Mutex<BTreeMap<LatestObjectCacheKey, CachedLatestObject>> {
    static CACHE: OnceLock<Mutex<BTreeMap<LatestObjectCacheKey, CachedLatestObject>>> =
        OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct LatestObjectCacheKey {
    site: String,
    days_back: i64,
}

#[derive(Clone, Debug)]
struct CachedLatestObject {
    object: S3Object,
    fetched_at: Instant,
}

#[derive(Debug, Deserialize)]
struct S3ListingXml {
    #[serde(rename = "Contents", default)]
    contents: Vec<S3ObjectXml>,
    #[serde(rename = "CommonPrefixes", default)]
    common_prefixes: Vec<CommonPrefixXml>,
}

impl From<S3ListingXml> for S3Listing {
    fn from(value: S3ListingXml) -> Self {
        Self {
            contents: value.contents.into_iter().map(Into::into).collect(),
            common_prefixes: value.common_prefixes.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CommonPrefix {
    prefix: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct S3Listing {
    contents: Vec<S3Object>,
    common_prefixes: Vec<CommonPrefix>,
}

#[derive(Debug, Deserialize)]
struct WeatherGovFeatureCollection {
    features: Vec<WeatherGovFeature>,
}

#[derive(Debug, Deserialize)]
struct WeatherGovFeature {
    geometry: Option<WeatherGovGeometry>,
    properties: WeatherGovProperties,
}

#[derive(Debug, Deserialize)]
struct WeatherGovGeometry {
    coordinates: Vec<f64>,
}

#[derive(Debug, Deserialize)]
struct WeatherGovProperties {
    id: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct S3ObjectXml {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "Size")]
    size: u64,
}

#[derive(Debug, Deserialize)]
struct CommonPrefixXml {
    #[serde(rename = "Prefix")]
    prefix: String,
}

impl From<S3ObjectXml> for S3Object {
    fn from(value: S3ObjectXml) -> Self {
        Self {
            key: value.key,
            size: value.size,
        }
    }
}

impl From<CommonPrefixXml> for CommonPrefix {
    fn from(value: CommonPrefixXml) -> Self {
        Self {
            prefix: value.prefix,
        }
    }
}

const FALLBACK_SITE_IDS: &[&str] = &[
    "KABR", "KABX", "KAKQ", "KAMA", "KAMX", "KAPX", "KARX", "KATX", "KBBX", "KBGM", "KBHX", "KBIS",
    "KBLX", "KBMX", "KBOX", "KBRO", "KBUF", "KBYX", "KCAE", "KCBW", "KCBX", "KCCX", "KCLE", "KCLX",
    "KCRP", "KCXX", "KCYS", "KDAX", "KDDC", "KDFX", "KDGX", "KDIX", "KDLH", "KDMX", "KDOX", "KDTX",
    "KDVN", "KDYX", "KEAX", "KEMX", "KENX", "KEOX", "KEPZ", "KESX", "KEVX", "KEWX", "KEYX", "KFCX",
    "KFDR", "KFDX", "KFFC", "KFSD", "KFSX", "KFTG", "KFWS", "KGGW", "KGJX", "KGLD", "KGRB", "KGRK",
    "KGRR", "KGSP", "KGWX", "KGYX", "KHDX", "KHGX", "KHNX", "KHPX", "KHTX", "KICT", "KICX", "KILN",
    "KILX", "KIND", "KINX", "KIWA", "KIWX", "KJAX", "KJGX", "KJKL", "KLBB", "KLCH", "KLGX", "KLNX",
    "KLOT", "KLRX", "KLSX", "KLTX", "KLVX", "KLWX", "KLZK", "KMAF", "KMAX", "KMBX", "KMHX", "KMKX",
    "KMLB", "KMOB", "KMPX", "KMQT", "KMRX", "KMSX", "KMTX", "KMUX", "KMVX", "KMXX", "KNKX", "KNQA",
    "KOAX", "KOHX", "KOKX", "KOTX", "KPAH", "KPBZ", "KPDT", "KPOE", "KPUX", "KRAX", "KRGX", "KRIW",
    "KRLX", "KRTX", "KSFX", "KSGF", "KSHV", "KSJT", "KSOX", "KSRX", "KTBW", "KTFX", "KTLH", "KTLX",
    "KTWX", "KTYX", "KUDX", "KUEX", "KVAX", "KVBX", "KVNX", "KVTX", "KVWX", "KYUX", "PABC", "PACG",
    "PAEC", "PAHG", "PAIH", "PAKC", "PAPD", "PHKI", "PHMO", "PHWA", "RKJK", "RKSG", "TADW", "TATL",
    "TBNA", "TBOS", "TCLT", "TCMH", "TCVG", "TDAL", "TDAY", "TDCA", "TDEN", "TDFW", "TDTW", "TEWR",
    "TFLL", "THOU", "TIAD", "TIAH", "TIDS", "TJFK", "TJUA", "TLAS", "TLVE", "TMCI", "TMCO", "TMDW",
    "TMEM", "TMIA", "TMKE", "TMSP", "TMSY", "TOKC", "TORD", "TPBI", "TPHL", "TPHX", "TPIT", "TRDU",
    "TSDF", "TSJU", "TSLC", "TSTL", "TTPA", "TTUL",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_can_carry_location() {
        let site = RadarSite::new("KTLX").with_location(
            Some("Norman".to_owned()),
            Some(35.333),
            Some(-97.278),
        );
        assert_eq!(site.name.as_deref(), Some("Norman"));
        assert_eq!(site.latitude_deg, Some(35.333));
    }

    #[test]
    fn fallback_has_many_sites() {
        assert!(fallback_sites().len() > 150);
    }
}
