# SIFIS-Home Demo Things repository

## Things

- [x] Lamp
- [x] On-Off Switch
- [x] Sink
- [x] Ticking Door
- [x] Ticking Sensor
- [ ] Cooker
- [ ] Oven
- [ ] Fridge

## Supported platforms

- [x] Unix-like

## Running the demos

Each demo is an independent binary, which can be run with:

```sh
cargo run --bin <demo_name>
```

where `<demo_name>` can be one of the following:

- `lamp`

### Getting more info

It is possible to obtain some useful information from the running webserver
using the
[`RUST_LOG`](https://rust-lang-nursery.github.io/rust-cookbook/development_tools/debugging/config_log.html)
environment variable. For instance, you can use the following to get some debug
information from the _lamp_ demo:

```sh
RUST_LOG=debug cargo run --bin lamp
```
