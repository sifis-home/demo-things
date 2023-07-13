use std::{future, ops::Not, path::PathBuf, pin::pin, time::Duration, vec};

use axum::{
    http::StatusCode,
    response::{IntoResponse, Json},
    Extension,
};
use clap::Parser;
use demo_things::{config_signal_loader, CliCommon, Simulation, SimulationStream, ThingBuilderExt};
use futures_concurrency::{future::Join, stream::Merge};
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use signal_hook::consts::SIGHUP;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::{IntervalStream, ReceiverStream};
use tower_http::cors::CorsLayer;
use tracing::{debug, info, trace};
use wot_serve::{
    servient::{BuildServient, HttpRouter, ServientSettings},
    Servient,
};
use wot_td::builder::{
    BuildableDataSchema, BuildableHumanReadableInfo, BuildableInteractionAffordance,
    ReadableWriteableDataSchema, SpecializableDataSchema,
};

const MESSAGE_QUEUE_LENGTH: usize = 16;

#[derive(Debug, Deserialize, Serialize)]
struct OvenStatus {
    open: bool,
    on: bool,
    temperature: f32,
    target_temperature: u8,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct OvenSimulation {
    #[serde(with = "humantime_serde")]
    wait: Duration,
    #[serde(skip_serializing_if = "Option::is_none")]
    open: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OvenConfig {
    initial: OvenStatus,
    simulation: Vec<OvenSimulation>,
    deltas: Deltas,
}

#[derive(Debug, Deserialize, Serialize)]
struct Deltas {
    increase: f32,
    door_open: f32,
    door_close: f32,
    target_range: f32,
}

#[derive(Debug)]
struct Oven {
    status: OvenStatus,
    simulation: vec::IntoIter<OvenSimulation>,
    deltas: Deltas,
    heating: bool,
}

#[derive(Debug, Parser)]
pub struct Cli {
    #[clap(flatten)]
    common: CliCommon,

    /// Dump a default configuration to the specified file and exit.
    #[clap(short, long)]
    dump: bool,

    /// The config TOML file for the oven.
    config: PathBuf,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    cli.common.setup_tracing();

    if cli.dump {
        let config = OvenConfig {
            initial: OvenStatus {
                open: false,
                on: false,
                temperature: 25.,
                target_temperature: 220,
            },
            deltas: Deltas {
                increase: 1.5,
                door_open: 1.2,
                door_close: 0.1,
                target_range: 5.,
            },
            simulation: vec![
                OvenSimulation {
                    wait: Duration::from_secs(3),
                    open: Some(true),
                },
                OvenSimulation {
                    wait: Duration::from_secs(8),
                    open: Some(false),
                },
                OvenSimulation {
                    wait: Duration::from_secs(15),
                    open: Some(true),
                },
                OvenSimulation {
                    wait: Duration::from_secs(3),
                    open: Some(false),
                },
            ],
        };

        let config = toml::to_vec(&config).unwrap();
        std::fs::write(&cli.config, config).expect("unable to dump config to file");
        println!(
            "Configuration successfully written to {}",
            cli.config.display()
        );
        return;
    };

    let oven: OvenConfig = {
        let config = std::fs::read(&cli.config).expect("unable to read config file");
        toml::from_slice(&config).expect("unable to parse config file")
    };

    let oven = Oven {
        status: oven.initial,
        simulation: oven.simulation.into_iter(),
        deltas: oven.deltas,
        heating: false,
    };

    let (message_sender, message_receiver) = mpsc::channel(MESSAGE_QUEUE_LENGTH);

    let app_state = AppState {
        message_sender: message_sender.clone(),
    };

    let addr = cli.common.socket_addr();
    let mut servient = Servient::builder(cli.common.title_or("My Oven"))
        .finish_extend()
        .id_from_cli(&cli.common)
        .attype("DoorSensor")
        .attype("Thermostat")
        .attype("OnOffSwitch")
        .base_from_cli(&cli.common)
        .description("A web connected oven")
        .security(|b| b.no_sec().with_key("nosec_sc").required())
        .form(|b| {
            b.href("/properties")
                .http_get(properties)
                .content_type("application/json")
                .op(wot_td::thing::FormOperation::ReadAllProperties)
        })
        .property("on", |b| {
            b.finish_extend_data_schema()
                .attype("OnOffProperty")
                .title("On/Off")
                .description("Whether the oven is turned on")
                .form(|b| {
                    b.href("/properties/on")
                        .http_get(get_on_property)
                        .http_put(put_on_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                        .op(wot_td::thing::FormOperation::WriteProperty)
                })
                .bool()
        })
        .property("door", |b| {
            b.finish_extend_data_schema()
                .attype("OpenProperty")
                .title("Open door")
                .description("Whether the door of the oven is open or closed")
                .form(|b| {
                    b.href("/properties/door")
                        .http_get(get_door_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                })
                .bool()
                .read_only()
        })
        .property("temperature", |b| {
            b.finish_extend_data_schema()
                .attype("TemperatureProperty")
                .title("Temperature")
                .description("The temperature")
                .form(|b| {
                    b.href("/properties/temperature")
                        .http_get(get_temperature_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                })
                .integer()
                .unit("degree celsius")
                .read_only()
        })
        .property("target_temperature", |b| {
            b.finish_extend_data_schema()
                .attype("TargetTemperatureProperty")
                .title("Target temperature")
                .description("The target temperature")
                .form(|b| {
                    b.href("/properties/target-temperature")
                        .http_get(get_target_temperature_property)
                        .http_put(put_target_temperature_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                        .op(wot_td::thing::FormOperation::WriteProperty)
                })
                .integer()
                .unit("degree celsius")
        })
        .http_bind(addr)
        .build_servient()
        .expect("cannot build Thing Descriptor for the oven");

    let cors = CorsLayer::new()
        .allow_methods(tower_http::cors::Any)
        .allow_origin(tower_http::cors::Any);

    servient.router = servient.router.layer(Extension(app_state)).layer(cors);

    let axum_future = async {
        tracing::debug!("listening on {}", addr);
        servient
            .serve()
            .await
            .unwrap_or_else(|err| panic!("unable to create web server on address {addr}: {err}"));
    };

    (handle_messages(oven, message_receiver, &cli), axum_future)
        .join()
        .await;
}

#[derive(Clone)]
struct AppState {
    message_sender: mpsc::Sender<Message>,
}

impl AppState {
    #[inline]
    async fn use_oneshot<F, T>(&self, f: F) -> T
    where
        F: FnOnce(oneshot::Sender<T>) -> Message,
    {
        let (sender, receiver) = oneshot::channel();
        self.send_message(f(sender)).await;

        receiver.await.unwrap()
    }

    async fn send_message(&self, message: Message) {
        self.message_sender
            .send(message)
            .await
            .expect("message channel should be open");
    }

    #[inline]
    async fn get_properties(&self) -> Properties {
        self.use_oneshot(Message::GetProperties).await
    }

    #[inline]
    async fn get_target_temperature(&self) -> u8 {
        self.use_oneshot(Message::GetTargetTemperature).await
    }

    #[inline]
    async fn get_door_is_open(&self) -> bool {
        self.use_oneshot(Message::GetOpen).await
    }

    #[inline]
    async fn get_temperature(&self) -> u8 {
        self.use_oneshot(Message::GetTemperature).await
    }

    #[inline]
    async fn get_is_on(&self) -> bool {
        self.use_oneshot(Message::GetIsOn).await
    }

    #[inline]
    async fn set_target_temperature(&self, value: u8) {
        self.send_message(Message::SetTargetTemperature(value))
            .await;
    }

    #[inline]
    async fn set_is_on(&self, value: bool) {
        self.send_message(Message::SetIsOn(value)).await;
    }
}

#[derive(Debug)]
enum Message {
    GetProperties(oneshot::Sender<Properties>),
    GetTargetTemperature(oneshot::Sender<u8>),
    GetTemperature(oneshot::Sender<u8>),
    GetOpen(oneshot::Sender<bool>),
    GetIsOn(oneshot::Sender<bool>),
    SetTargetTemperature(u8),
    SetIsOn(bool),
}

async fn handle_messages(oven: Oven, receiver: mpsc::Receiver<Message>, cli: &Cli) {
    #[derive(Debug)]
    enum Event {
        Message(Message),
        Config(OvenConfig),
        NextStatus(OvenSimulationOutput),
        Tick,
        Stop,
    }

    let create_interval_stream = || {
        IntervalStream::new(tokio::time::interval(Duration::from_millis(10))).map(|_| Event::Tick)
    };
    let create_simulation_stream =
        |simulation| SimulationStream::new(simulation).map(Event::NextStatus);

    let Oven {
        mut status,
        simulation,
        mut deltas,
        mut heating,
    } = oven;

    let mut csl = config_signal_loader([SIGHUP], &cli.config);
    let mut simulation_stream = create_simulation_stream(simulation);

    let mut interval_stream = create_interval_stream();
    let mut receiver_stream = ReceiverStream::new(receiver)
        .map(Event::Message)
        .chain(stream::once(future::ready(Event::Stop)));
    let mut csl_stream = csl.stream.by_ref().map(Event::Config);

    'outer: loop {
        let mut events_stream = pin!((
            &mut receiver_stream,
            &mut csl_stream,
            &mut interval_stream,
            simulation_stream,
        )
            .merge());

        loop {
            let Some(event) = events_stream.next().await else {
                break 'outer;
            };

            match event {
                Event::Message(message) => {
                    handle_message(
                        message,
                        &mut status.open,
                        status.temperature,
                        &mut status.target_temperature,
                        &mut status.on,
                    )
                    .await
                }
                Event::Tick => handle_tick(
                    status.open,
                    status.target_temperature,
                    status.on,
                    &mut status.temperature,
                    &mut heating,
                    &deltas,
                ),
                Event::Config(config) => {
                    info!("New config obtained. Resetting sensor statuses to new config.");

                    status = config.initial;
                    deltas = config.deltas;
                    simulation_stream = create_simulation_stream(config.simulation.into_iter());

                    interval_stream = create_interval_stream();
                    // Skip immediate tick
                    interval_stream.next().await;
                    break;
                }
                Event::NextStatus(next_status) => {
                    debug!("Got next status from simulation: {next_status:#?}");

                    if let Some(open) = next_status.open {
                        status.open = open;
                        if open {
                            info!("Door is now open");
                        } else {
                            info!("Door is now closed");
                        }
                    }
                }
                Event::Stop => break 'outer,
            }
        }
    }
}

impl Simulation for OvenSimulation {
    type Output = OvenSimulationOutput;

    fn output_and_wait(self) -> (Self::Output, Duration) {
        let Self { wait, open } = self;
        let output = OvenSimulationOutput { open };
        (output, wait)
    }
}

#[derive(Debug)]
struct OvenSimulationOutput {
    open: Option<bool>,
}

fn handle_tick(
    door_is_open: bool,
    target_temperature: u8,
    is_on: bool,
    temperature: &mut f32,
    heating: &mut bool,
    deltas: &Deltas,
) {
    #[derive(Debug, Clone, Copy)]
    enum TargetRange {
        Below,
        InRange,
        Above,
    }

    let mut temp_delta = if door_is_open {
        -deltas.door_open
    } else {
        -deltas.door_close
    };

    if is_on {
        let target_temperature = f32::from(target_temperature);
        let target_range = if *temperature < target_temperature - deltas.target_range {
            TargetRange::Below
        } else if *temperature <= target_temperature + deltas.target_range {
            TargetRange::InRange
        } else {
            TargetRange::Above
        };

        if matches!(
            (*heating, target_range),
            (true, TargetRange::Above) | (false, TargetRange::Below)
        ) {
            *heating = heating.not();
        }
    } else {
        *heating = false;
    }

    if *heating {
        temp_delta += deltas.increase;
    }

    let final_temperature = f32::clamp(*temperature + temp_delta, 25., 250.);
    if final_temperature != *temperature {
        trace!("Temperature: {temperature:.2} -> {final_temperature:.2}",);
    }
    *temperature = final_temperature;
}

async fn handle_message(
    message: Message,
    door_is_open: &mut bool,
    temperature: f32,
    target_temperature: &mut u8,
    is_on: &mut bool,
) {
    use Message::*;

    match message {
        GetProperties(sender) => {
            let properties = Properties {
                open: *door_is_open,
                temperature: temperature as u8,
                target_temperature: *target_temperature,
            };
            sender.send(properties).unwrap();
        }
        GetTargetTemperature(sender) => sender.send(*target_temperature).unwrap(),
        GetOpen(sender) => sender.send(*door_is_open).unwrap(),
        SetTargetTemperature(value) => {
            info!("Target temperature: {} -> {value}", *target_temperature);
            *target_temperature = value;
        }
        GetTemperature(sender) => sender.send(temperature as u8).unwrap(),
        GetIsOn(sender) => sender.send(*is_on).unwrap(),
        SetIsOn(value) => {
            info!("On status: {} -> {value}", *is_on);
            *is_on = value;
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Properties {
    open: bool,
    temperature: u8,
    target_temperature: u8,
}

async fn properties(Extension(state): Extension<AppState>) -> Json<Properties> {
    let properties = state.get_properties().await;

    Json(properties)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Property {
    Temperature(i8),
    TargetTemperature(i8),
    Open(bool),
    On(bool),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PropertyName {
    Temperature,
    TargetTemperature,
    Open,
    On,
}

async fn get_door_property(Extension(app): Extension<AppState>) -> Json<bool> {
    let door_is_open = app.get_door_is_open().await;
    Json(door_is_open)
}

async fn get_temperature_property(Extension(app): Extension<AppState>) -> Json<u8> {
    let temperature = app.get_temperature().await;
    Json(temperature)
}

async fn get_target_temperature_property(Extension(app): Extension<AppState>) -> Json<u8> {
    let target_temperature = app.get_target_temperature().await;
    Json(target_temperature)
}

async fn get_on_property(Extension(app): Extension<AppState>) -> Json<bool> {
    let is_on = app.get_is_on().await;
    Json(is_on)
}

async fn put_target_temperature_property(
    Extension(app): Extension<AppState>,
    Json(value): Json<u8>,
) -> impl IntoResponse {
    app.set_target_temperature(value).await;
    StatusCode::NO_CONTENT
}

async fn put_on_property(
    Extension(app): Extension<AppState>,
    Json(value): Json<bool>,
) -> impl IntoResponse {
    app.set_is_on(value).await;
    StatusCode::NO_CONTENT
}
