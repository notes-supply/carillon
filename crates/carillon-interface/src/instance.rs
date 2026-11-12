use anyhow::{Result, bail};
use carillon_installation::config::LoadedConfig;

/// The IDs of the registry entries `program` serves, in registry order.
#[must_use]
pub fn instances<'a>(config: &'a LoadedConfig, program: &str) -> Vec<&'a str> {
    let providers = config.value.providers.iter();
    providers
        .filter(|provider| provider.program == program)
        .map(|provider| provider.id.as_str())
        .collect()
}

/// The entry of `program` an operator action names, or the only one
/// registered. Anything else is a choice for the operator.
///
/// # Errors
///
/// Returns an error when the named entry is another program's, or when none
/// is named and the registry has no entry of `program` or several.
pub fn instance(config: &LoadedConfig, program: &str, explicit: Option<String>) -> Result<String> {
    if let Some(explicit) = explicit {
        let mut entries = config.value.providers.iter();
        let other = entries
            .find(|entry| entry.id == explicit)
            .map(|entry| &entry.program);
        if let Some(other) = other.filter(|other| *other != program) {
            bail!("{explicit} is not an instance of \"{program}\": its program is \"{other}\"");
        }
        return Ok(explicit);
    }
    match instances(config, program).as_slice() {
        [only] => Ok((*only).to_owned()),
        [] => bail!(
            "carillon.toml registers no provider with program = \"{program}\"; `carillon setup \
             --interface {program}` adds one"
        ),
        several => bail!(
            "several providers with program = \"{program}\" are registered; choose one with \
             --provider from: {}",
            several.join(", ")
        ),
    }
}
