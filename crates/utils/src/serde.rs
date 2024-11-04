use reqwest::Url;
use serde::{Deserialize, Deserializer, Serializer};

pub mod as_u16 {
    use http::StatusCode;
    use serde::{de::Deserializer, Deserialize, Serializer};

    pub fn serialize<S>(x: &StatusCode, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_u16(x.as_u16())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<StatusCode, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value: u16 = Deserialize::deserialize(deserializer)?;
        StatusCode::from_u16(value).map_err(serde::de::Error::custom)
    }
}

pub mod axum_as_u16 {
    use axum::http::StatusCode; // Update the import to axum::http::StatusCode
    use serde::{de::Deserializer, Deserialize, Serializer};

    pub fn serialize<S>(x: &StatusCode, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_u16(x.as_u16())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<StatusCode, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value: u16 = Deserialize::deserialize(deserializer)?;
        StatusCode::from_u16(value).map_err(serde::de::Error::custom)
    }
}

/// Custom deserializer to convert a string to a url::Url
pub fn deserialize_url<'de, D>(deserializer: D) -> Result<Url, D::Error>
where
    D: Deserializer<'de>,
{
    let url_str: String = Deserialize::deserialize(deserializer)?;
    Url::parse(&url_str).map_err(serde::de::Error::custom)
}

/// Custom serializer to convert a url::Url to a string
pub fn serialize_url<S>(url: &Url, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(url.as_str())
}

pub const fn default_bool<const B: bool>() -> bool {
    B
}
