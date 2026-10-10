# Card colours: a custom Discord module

The bundled Discord module with one change: each work card gets a colour
stripe for its job's state (blurple while working, green when answered,
yellow when delivery is uncertain, red when stopped). Everything else is the
bundled module's own handling, through
[`carillon_discord_module::handle`](../../crates/carillon-discord-module/src/lib.rs),
and `carillon_discord_module::export!` gives it the provider's entry points.

The release ships it built, so you can try it without a toolchain:

```sh
carillon discord module enable ~/.carillon/app/lib/examples/card-colours.wasm
carillon discord module disable    # back to the bundled module
```

To change it, edit [`src/lib.rs`](src/lib.rs), then build and enable your copy
from the repository:

```sh
mise run build:module
carillon discord module enable "$PWD/target/wasm32-unknown-unknown/wasm/carillon_card_colours.wasm"
# after each rebuild:
carillon discord settings apply
```

Enabling validates the module before switching. A rejected candidate leaves
the current module serving; disabling returns to the bundled module.
