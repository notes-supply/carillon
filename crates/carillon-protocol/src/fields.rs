//! Deserializers for the wire types' fields.

use serde::Deserialize;

// Serde otherwise treats Option fields as optional even without #[serde(default)].
pub(crate) fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

pub(crate) fn optional_value<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

pub(crate) fn nonempty_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.is_empty() {
        return Err(serde::de::Error::custom("expected a nonempty string"));
    }
    Ok(value)
}

/// A provider's ID, as TypeScript's `ProviderIdentitySchema` takes it: 1 to
/// 128 ASCII letters, digits, `_` and `-`.
pub(crate) fn provider_identity<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(serde::de::Error::custom(
            "expected a provider ID of 1 to 128 letters, digits, `_` or `-`",
        ));
    }
    Ok(value)
}

pub(crate) fn optional_nonempty_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    nonempty_string(deserializer).map(Some)
}
