# Registry mapping rules

One file per adjacent protocol pair, `<older>-<newer>.json`, written by hand.
The format and the exact mapping algorithm (including block-state patterns
such as `oak_slab[type=double]`) are documented in `src/mapping.rs`.

```json
{"item": {"renames": {"scute": "turtle_scute"},
          "up": {"removed_in_newer": "newer_stand_in"},
          "down": {"added_in_newer": "older_stand_in"},
          "up_default": "stone", "down_default": "stone"}}
```

See what is still unmapped for a pair, or for every pair:

```sh
cargo run -p rift-translation --example mapping_report -- 754 755 [registry]
cargo run -p rift-translation --example mapping_report -- --all
```

`cargo test -p rift-translation` fails on a rules file that does not parse,
names a registry or entry that does not exist, or is named for protocols that
are not adjacent. Renames are for entries that stayed the same thing; a step
still has to convert any payload whose layout changed.
