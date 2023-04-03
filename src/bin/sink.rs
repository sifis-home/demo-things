use clap::Parser;
use demo_things::CliCommon;
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, ops::Not, time::Duration};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::{
    join, select,
    sync::{broadcast, mpsc, oneshot},
};
use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt};
use wot_serve::{
    servient::{BuildServient, HttpRouter, ServientSettings},
    Servient,
};

use axum::{
    http::StatusCode,
    response::{sse, sse::KeepAlive, IntoResponse, Json, Sse},
    Extension,
};
use wot_td::builder::{
    BuildableDataSchema, BuildableHumanReadableInfo, BuildableInteractionAffordance,
    IntegerDataSchemaBuilderLike, ReadableWriteableDataSchema, SpecializableDataSchema,
};

struct Sink {
    is_draining: bool,
    flow: u8,
    temperature: u8,
    level: f32,
    fill_rate: f32,
    drain_rate: f32,
}

const MESSAGE_QUEUE_LENGTH: usize = 16;

#[derive(Parser)]
struct Cli {
    #[clap(flatten)]
    common: CliCommon,

    /// The fill rate.
    #[clap(short, long, default_value_t = 1.)]
    fill: f32,

    /// The drain rate.
    #[clap(short, long, default_value_t = 0.8)]
    drain: f32,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    cli.common.setup_tracing();

    let sink = Sink {
        is_draining: true,
        flow: 0,
        temperature: 37,
        level: 0.,
        fill_rate: cli.fill,
        drain_rate: cli.drain,
    };

    let (message_sender, message_receiver) = mpsc::channel(MESSAGE_QUEUE_LENGTH);
    let (event_sender, _event_receiver) = broadcast::channel(MESSAGE_QUEUE_LENGTH);

    let app_state = AppState {
        message_sender: message_sender.clone(),
        event_sender: event_sender.clone(),
    };

    let addr = cli.common.socket_addr();
    let mut servient = Servient::builder("My Sink")
        .finish_extend()
        .id("urn:dev:ops:my-sink-1234")
        .attype("OnOffSwitch")
        .description("A web connected sink")
        .security(|b| b.no_sec().with_key("nosec_sc").required())
        .form(|b| {
            b.href("/properties")
                .http_get(properties)
                .content_type("application/json")
                .op(wot_td::thing::FormOperation::ReadAllProperties)
        })
        .form(|b| {
            b.href("/events")
                .http_get(all_events)
                .op(wot_td::thing::FormOperation::SubscribeAllEvents)
                .op(wot_td::thing::FormOperation::UnsubscribeAllEvents)
                .subprotocol("sse")
        })
        .property("drain", |b| {
            b.finish_extend_data_schema()
                .attype("OnOffProperty")
                .title("Drain Open/Close")
                .description("Whether the drain is open (on) or closed (off)")
                .form(|b| {
                    b.href("/properties/drain")
                        .http_get(get_drain_property)
                        .http_put(put_drain_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                        .op(wot_td::thing::FormOperation::WriteProperty)
                })
                .bool()
        })
        .property("flow", |b| {
            b.finish_extend_data_schema()
                .title("Flow")
                .description("The percentage of flow from 0-100")
                .form(|b| {
                    b.href("/properties/flow")
                        .http_get(get_flow_property)
                        .http_put(put_flow_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                        .op(wot_td::thing::FormOperation::WriteProperty)
                })
                .integer()
                .minimum(0)
                .maximum(100)
                .unit("percent")
        })
        .property("temperature", |b| {
            b.finish_extend_data_schema()
                .title("Temperature")
                .description("The temperature expressed in Celsius degrees")
                .form(|b| {
                    b.href("/properties/temperature")
                        .http_get(get_temperature_property)
                        .http_put(put_temperature_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                        .op(wot_td::thing::FormOperation::WriteProperty)
                })
                .integer()
                .minimum(10)
                .maximum(80)
                .unit("C°")
        })
        .property("level", |b| {
            b.finish_extend_data_schema()
                .title("Level")
                .description("The level of the water expressed as percentage")
                .form(|b| {
                    b.href("/properties/level")
                        .http_get(get_level_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                })
                .integer()
                .minimum(0)
                .maximum(100)
                .unit("percentage")
                .read_only()
        })
        .event("leak", |b| {
            b.description("The sink is full and water is still flowing")
                .form(|b| {
                    b.href("/events/leak")
                        .http_get(leak_events)
                        .op(wot_td::thing::FormOperation::SubscribeEvent)
                        .op(wot_td::thing::FormOperation::UnsubscribeEvent)
                        .subprotocol("sse")
                })
        })
        .http_bind(addr)
        .build_servient()
        .expect("cannot build Thing Descriptor for the sink");

    servient.router = servient.router.layer(Extension(app_state));

    let axum_future = async {
        tracing::debug!("listening on {}", addr);
        servient
            .serve()
            .await
            .unwrap_or_else(|err| panic!("unable to create web server on address {addr}: {err}"));
    };

    join!(
        handle_messages(sink, message_receiver, event_sender),
        axum_future
    );
}

#[derive(Clone)]
struct AppState {
    message_sender: mpsc::Sender<Message>,
    event_sender: broadcast::Sender<Event>,
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
    async fn get_flow(&self) -> u8 {
        self.use_oneshot(Message::GetFlow).await
    }

    #[inline]
    async fn get_drain(&self) -> bool {
        self.use_oneshot(Message::GetDrain).await
    }

    #[inline]
    async fn get_temperature(&self) -> u8 {
        self.use_oneshot(Message::GetTemperature).await
    }

    #[inline]
    async fn get_level(&self) -> u8 {
        self.use_oneshot(Message::GetLevel).await
    }

    #[inline]
    async fn set_flow(&self, value: u8) {
        self.send_message(Message::SetFlow(value)).await;
    }

    #[inline]
    async fn set_drain(&self, value: bool) {
        self.send_message(Message::SetDrain(value)).await;
    }

    #[inline]
    async fn set_temperature(&self, value: u8) {
        self.send_message(Message::SetTemperature(value)).await;
    }
}

#[derive(Debug)]
enum Message {
    GetProperties(oneshot::Sender<Properties>),
    GetDrain(oneshot::Sender<bool>),
    GetFlow(oneshot::Sender<u8>),
    GetTemperature(oneshot::Sender<u8>),
    GetLevel(oneshot::Sender<u8>),
    SetDrain(bool),
    SetFlow(u8),
    SetTemperature(u8),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
enum Event {
    Leak {
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
}

async fn handle_messages(
    sink: Sink,
    mut receiver: mpsc::Receiver<Message>,
    event_sender: broadcast::Sender<Event>,
) {
    let Sink {
        mut is_draining,
        mut flow,
        mut temperature,
        mut level,
        fill_rate,
        drain_rate,
    } = sink;

    let mut interval = tokio::time::interval(Duration::from_millis(10));

    loop {
        select! {
            message = receiver.recv() => {
                match message {
                    Some(message) => handle_message(
                        message,
                        &mut is_draining,
                        &mut flow,
                        &mut temperature,
                        level,
                    ).await,

                    None => break,
                }
            }

            _ = interval.tick() => {
                handle_tick(is_draining, flow, fill_rate, drain_rate, &mut level, &event_sender);
            }
        }
    }
}

fn handle_tick(
    is_draining: bool,
    flow: u8,
    fill_rate: f32,
    drain_rate: f32,
    level: &mut f32,
    event_sender: &broadcast::Sender<Event>,
) {
    let drain = is_draining.then(|| -drain_rate).unwrap_or(0.);
    let fill = f32::from(flow) / 100. * fill_rate;

    let new_level = *level + drain + fill;
    *level = if new_level > 100. {
        event_sender
            .send(Event::Leak {
                timestamp: OffsetDateTime::now_utc(),
            })
            .expect("events channel should be open");
        100.
    } else {
        new_level.max(0.)
    };
}

async fn handle_message(
    message: Message,
    is_draining: &mut bool,
    flow: &mut u8,
    temperature: &mut u8,
    level: f32,
) {
    use Message::*;

    match message {
        GetProperties(sender) => {
            let properties = Properties {
                flow: *flow,
                temperature: *temperature,
                drain: *is_draining,
                level: level as u8,
            };
            sender.send(properties).unwrap();
        }
        GetFlow(sender) => sender.send(*flow).unwrap(),
        GetDrain(sender) => sender.send(*is_draining).unwrap(),
        GetTemperature(sender) => sender.send(*temperature).unwrap(),
        GetLevel(sender) => sender.send(level as u8).unwrap(),
        SetFlow(value) => *flow = value,
        SetDrain(value) => *is_draining = value,
        SetTemperature(value) => *temperature = value,
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Properties {
    flow: u8,
    temperature: u8,
    drain: bool,
    level: u8,
}

async fn properties(Extension(state): Extension<AppState>) -> Json<Properties> {
    let properties = state.get_properties().await;

    Json(properties)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Property {
    Flow(u8),
    Drain(bool),
    Temperature(u8),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PropertyName {
    Flow,
    Drain,
    Temperature,
}

fn handle_sse_stream<F>(
    Extension(state): Extension<AppState>,
    mut f: F,
) -> Sse<impl Stream<Item = Result<sse::Event, Infallible>>>
where
    F: FnMut(Event) -> sse::Event + Send + 'static,
{
    let receiver = BroadcastStream::new(state.event_sender.subscribe())
        .filter_map(move |ev| ev.ok().map(|event| Ok(f(event))));

    Sse::new(receiver).keep_alive(KeepAlive::default())
}

fn handle_leak_event(event: Event) -> sse::Event {
    match event {
        Event::Leak { timestamp } => {
            let mut event = sse::Event::default().event("leak");
            if let Ok(timestamp) = timestamp.format(&Rfc3339) {
                event = event.id(timestamp);
            }
            event
        }
    }
}

#[inline]
async fn all_events(
    extension: Extension<AppState>,
) -> Sse<impl Stream<Item = Result<sse::Event, Infallible>>> {
    handle_sse_stream(extension, handle_leak_event)
}

#[inline]
async fn leak_events(
    extension: Extension<AppState>,
) -> Sse<impl Stream<Item = Result<sse::Event, Infallible>>> {
    handle_sse_stream(extension, handle_leak_event)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum EventName {
    Overheated,
}

async fn get_drain_property(Extension(app): Extension<AppState>) -> Json<bool> {
    let is_on = app.get_drain().await;
    Json(is_on)
}

async fn put_drain_property(
    Json(value): Json<bool>,
    Extension(app): Extension<AppState>,
) -> impl IntoResponse {
    app.set_drain(value).await;
    StatusCode::NO_CONTENT
}

async fn get_flow_property(Extension(app): Extension<AppState>) -> Json<u8> {
    let flow = app.get_flow().await;
    Json(flow)
}

async fn put_flow_property(
    Json(value): Json<u8>,
    Extension(app): Extension<AppState>,
) -> impl IntoResponse {
    if value > 100 {
        return StatusCode::BAD_REQUEST;
    }

    app.set_flow(value).await;
    StatusCode::NO_CONTENT
}

async fn get_temperature_property(Extension(app): Extension<AppState>) -> Json<u8> {
    let temperature = app.get_temperature().await;
    Json(temperature)
}

async fn put_temperature_property(
    Json(value): Json<u8>,
    Extension(app): Extension<AppState>,
) -> impl IntoResponse {
    if (10..=80).contains(&value).not() {
        return StatusCode::BAD_REQUEST;
    }

    app.set_temperature(value).await;
    StatusCode::NO_CONTENT
}

async fn get_level_property(Extension(app): Extension<AppState>) -> Json<u8> {
    let level = app.get_level().await;
    Json(level)
}
