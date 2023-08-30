use clap::Parser;
use demo_things::{CliCommon, ThingBuilderExt};
use futures_concurrency::future::Join;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tower_http::cors::CorsLayer;
use wot_serve::{
    servient::{BuildServient, HttpRouter, ServientSettings},
    Servient,
};

use axum::{
    http::StatusCode,
    response::{IntoResponse, Json},
    Extension,
};
use wot_td::builder::{
    BuildableDataSchema, BuildableHumanReadableInfo, BuildableInteractionAffordance,
    IntegerDataSchemaBuilderLike, SpecializableDataSchema,
};

struct Thing {
    is_on: bool,
    brightness: u8,
}

const MESSAGE_QUEUE_LENGTH: usize = 16;

#[derive(Parser)]
struct Cli {
    #[clap(flatten)]
    common: CliCommon,

    /// Add the Light @type to the switch
    #[clap(long)]
    light: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    cli.common.setup_tracing();

    let thing = Thing {
        is_on: true,
        brightness: 50,
    };

    let (message_sender, message_receiver) = mpsc::channel(MESSAGE_QUEUE_LENGTH);

    let app_state = AppState {
        message_sender: message_sender.clone(),
    };

    let addr = cli.common.socket_addr();
    let mut thing_builder = Servient::builder("On-Off Switch with Brightness")
        .finish_extend()
        .id("urn:dev:ops:on-off-1234")
        .attype("OnOffSwitch");
    if cli.light {
        thing_builder = thing_builder.attype("Light");
    }
    let mut servient = thing_builder
        .base_from_cli(&cli.common)
        .security(|b| b.no_sec().with_key("nosec_sc").required())
        .property("on", |b| {
            b.finish_extend_data_schema()
                .attype("OnOffProperty")
                .title("On/Off")
                .description("Whether the switch is turned on")
                .form(|b| {
                    b.href("/properties/on")
                        .http_get(get_on_property)
                        .http_put(put_on_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                        .op(wot_td::thing::FormOperation::WriteProperty)
                })
                .bool()
        })
        .property("brightness", |b| {
            b.finish_extend_data_schema()
                .attype("BrightnessProperty")
                .title("Brightness")
                .form(|b| {
                    b.href("/properties/brightness")
                        .http_get(get_brightness_property)
                        .http_put(put_brightness_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                        .op(wot_td::thing::FormOperation::WriteProperty)
                })
                .integer()
                .minimum(0)
                .maximum(100)
                .unit("percent")
        })
        .http_bind(addr)
        .build_servient()
        .expect("cannot build Thing Descriptor for the on-off switch with brightness");

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

    (handle_messages(thing, message_receiver), axum_future)
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
    async fn get_is_on(&self) -> bool {
        self.use_oneshot(Message::GetIsOn).await
    }

    #[inline]
    async fn set_is_on(&self, value: bool) {
        self.send_message(Message::SetIsOn(value)).await;
    }

    #[inline]
    async fn get_brightness(&self) -> u8 {
        self.use_oneshot(Message::GetBrightness).await
    }

    #[inline]
    async fn set_brightness(&self, value: u8) {
        self.send_message(Message::SetBrightness(value)).await;
    }
}

#[derive(Debug)]
enum Message {
    GetIsOn(oneshot::Sender<bool>),
    SetIsOn(bool),
    GetBrightness(oneshot::Sender<u8>),
    SetBrightness(u8),
}

async fn handle_messages(thing: Thing, mut receiver: mpsc::Receiver<Message>) {
    let Thing {
        mut is_on,
        mut brightness,
    } = thing;

    while let Some(message) = receiver.recv().await {
        handle_message(message, &mut is_on, &mut brightness);
    }
}

fn handle_message(message: Message, is_on: &mut bool, brightness: &mut u8) {
    use Message::{GetBrightness, GetIsOn, SetBrightness, SetIsOn};

    match message {
        GetIsOn(sender) => sender.send(*is_on).unwrap(),
        SetIsOn(value) => *is_on = value,
        GetBrightness(sender) => sender.send(*brightness).unwrap(),
        SetBrightness(value) => *brightness = value,
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Properties {
    brightness: u8,
    on: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Property {
    On(bool),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PropertyName {
    On,
}

async fn get_on_property(Extension(app): Extension<AppState>) -> Json<bool> {
    let is_on = app.get_is_on().await;
    Json(is_on)
}

async fn put_on_property(
    Extension(app): Extension<AppState>,
    Json(value): Json<bool>,
) -> impl IntoResponse {
    app.set_is_on(value).await;
    StatusCode::NO_CONTENT
}

async fn get_brightness_property(Extension(app): Extension<AppState>) -> Json<u8> {
    let brightness = app.get_brightness().await;
    Json(brightness)
}

async fn put_brightness_property(
    Extension(app): Extension<AppState>,
    Json(value): Json<u8>,
) -> impl IntoResponse {
    app.set_brightness(value).await;
    StatusCode::NO_CONTENT
}
