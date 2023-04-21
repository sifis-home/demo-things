# SIFIS-Home Demo Things repository

## Things

- [x] Lamp
- [x] On-Off Switch
- [x] Sink
- [x] Ticking Door
- [x] Ticking Sensor
- [ ] Cooker
- [x] Oven
- [x] Fridge

## Supported platforms

- [x] Unix-like

## Running the demos

Each demo is an independent binary, which can be run with:

```sh
cargo run --bin <demo_name>
```

where `<demo_name>` can be one of the following:

- fridge
- lamp
- on-off-switch-brightness-fade
- on-off-switch-brightness-float-fade
- on-off-switch-brightness-float
- on-off-switch-brightness
- on-off-switch-fade
- on-off-switch
- on-off-switch-toggle
- oven
- sink
- ticking-door
- ticking-sensor

### Getting more info

It is possible to obtain some useful information from the running webserver
using the
[`RUST_LOG`](https://rust-lang-nursery.github.io/rust-cookbook/development_tools/debugging/config_log.html)
environment variable. For instance, you can use the following to get some debug
information from a demo:

```sh
RUST_LOG=debug cargo run --bin <demo_name>
```
