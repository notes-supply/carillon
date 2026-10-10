//! The Discord module the release bundles: the module library's own
//! handling, given the provider's entry points. Packaging installs it as
//! `lib/carillon_discord_module.wasm`.

carillon_discord_module::export!(carillon_discord_module::handle);
