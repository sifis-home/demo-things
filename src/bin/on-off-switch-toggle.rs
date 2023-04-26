use clap::Parser;
use demo_things::{CliCommon, ThingBuilderExt};
use futures_concurrency::future::Join;
use serde::{Deserialize, Serialize};
use std::ops::Not;
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
    BuildableHumanReadableInfo, BuildableInteractionAffordance, SpecializableDataSchema,
};

struct Thing {
    is_on: bool,
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

    let thing = Thing { is_on: true };

    let (message_sender, message_receiver) = mpsc::channel(MESSAGE_QUEUE_LENGTH);

    let app_state = AppState {
        message_sender: message_sender.clone(),
    };

    let addr = cli.common.socket_addr();
    let mut thing_builder = Servient::builder("On-Off Switch")
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
        .action("toggle", |b| {
            b.attype("ToggleAction")
                .title("Toggle On/Off")
                .description("Invert the on/off status")
                .form(|b| {
                    b.href("/actions/toggle")
                        .http_post(post_toggle)
                        .op(wot_td::thing::FormOperation::InvokeAction)
                })
                .synchronous(true)
        })
        .http_bind(addr)
        .build_servient()
        .expect("cannot build Thing Descriptor for the on-off switch");

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
    async fn toggle(&self) {
        self.send_message(Message::Toggle).await;
    }
}

#[derive(Debug)]
enum Message {
    GetIsOn(oneshot::Sender<bool>),
    SetIsOn(bool),
    Toggle,
}

async fn handle_messages(thing: Thing, mut receiver: mpsc::Receiver<Message>) {
    let Thing { mut is_on } = thing;

    while let Some(message) = receiver.recv().await {
        handle_message(message, &mut is_on).await
    }
}

async fn handle_message(message: Message, is_on: &mut bool) {
    use Message::*;

    match message {
        GetIsOn(sender) => sender.send(*is_on).unwrap(),
        SetIsOn(value) => *is_on = value,
        Toggle => *is_on = is_on.not(),
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
    Json(value): Json<bool>,
    Extension(app): Extension<AppState>,
) -> impl IntoResponse {
    app.set_is_on(value).await;
    StatusCode::NO_CONTENT
}

async fn post_toggle(Extension(app): Extension<AppState>) -> impl IntoResponse {
    app.toggle().await;
    StatusCode::OK
}
