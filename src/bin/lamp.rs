use http_api_problem::HttpApiProblem;

use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::{
    join,
    sync::{broadcast, mpsc, oneshot},
};
use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt};
use uuid::Uuid;
use wot_serve::{
    servient::{BuildServient, HttpRouter, ServientSettings},
    Servient,
};

use axum::{
    extract::Path,
    http::StatusCode,
    response::{
        sse::{self, KeepAlive},
        IntoResponse, Json, Sse,
    },
    Extension,
};
use wot_td::builder::{
    BuildableDataSchema, BuildableHumanReadableInfo, BuildableInteractionAffordance,
    IntegerDataSchemaBuilderLike, ObjectDataSchemaBuilderLike, SpecializableDataSchema,
};

struct Lamp {
    _is_on: bool,
    _brightness: u8,
    _actions: Vec<StoredAction>,
    _temperature: u16,
}

const MESSAGE_QUEUE_LENGTH: usize = 16;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt::init();

    let lamp = Lamp {
        _is_on: true,
        _brightness: 50,
        _actions: Default::default(),
        _temperature: 25,
    };

    let (message_sender, message_receiver) = mpsc::channel(MESSAGE_QUEUE_LENGTH);
    let (event_sender, _event_receiver) = broadcast::channel(MESSAGE_QUEUE_LENGTH);

    let app_state = AppState {
        message_sender: message_sender.clone(),
        event_sender: event_sender.clone(),
    };

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    let mut servient = Servient::builder("My Lamp")
        .finish_extend()
        .id("urn:dev:ops:my-lamp-1234")
        .attype("OnOffSwitch")
        .attype("Light")
        .description("A web connected lamp")
        .security(|b| b.no_sec().with_key("nosec_sc").required())
        .form(|b| {
            b.href("/properties")
                .http_get(properties)
                .content_type("application/json")
                .op(wot_td::thing::FormOperation::ReadAllProperties)
        })
        .form(|b| {
            b.href("/properties/observe")
                .http_get(observe_properties)
                .op(wot_td::thing::FormOperation::ObserveAllProperties)
                .op(wot_td::thing::FormOperation::UnobserveAllProperties)
                .subprotocol("sse")
        })
        .form(|b| {
            b.href("/actions")
                .http_get(get_actions)
                .content_type("application/json")
                .op(wot_td::thing::FormOperation::QueryAllActions)
        })
        .form(|b| {
            b.href("/events")
                .http_get(all_events)
                .op(wot_td::thing::FormOperation::SubscribeAllEvents)
                .op(wot_td::thing::FormOperation::UnsubscribeAllEvents)
                .subprotocol("sse")
        })
        .property("on", |b| {
            b.finish_extend_data_schema()
                .attype("OnOffProperty")
                .title("On/Off")
                .description("Whether the lamp is turned on")
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
                .description("The level of light from 0-100")
                .form(|b| {
                    b.href("/properties/brightness")
                        .http_get(get_brightness_property)
                        .http_put(put_brightness_property)
                        .op(wot_td::thing::FormOperation::ReadProperty)
                        .op(wot_td::thing::FormOperation::WriteProperty)
                })
                .form(|b| {
                    b.href("/properties/brightness/observe")
                        .http_get(observe_brightness)
                        .op(wot_td::thing::FormOperation::ObserveProperty)
                        .op(wot_td::thing::FormOperation::UnobserveProperty)
                        .subprotocol("sse")
                })
                .observable(true)
                .integer()
                .minimum(0)
                .maximum(100)
                .unit("percent")
        })
        .action("fade", |b| {
            b.title("Fade")
                .description("Fade the lamp to a given level")
                .form(|b| {
                    b.href("/actions/fade")
                        .http_post(post_fade_action)
                        .op(wot_td::thing::FormOperation::InvokeAction)
                })
                .form(|b| {
                    b.href("/actions/fade/{action_id}")
                        .http_get(get_fade_action)
                        .op(wot_td::thing::FormOperation::QueryAction)
                        .http_delete(delete_fade_action)
                        .op(wot_td::thing::FormOperation::CancelAction)
                })
                .uri_variable("action_id", |b| {
                    b.finish_extend()
                        .description("Identifier of the ongoing action")
                        .string()
                })
                .input(|b| {
                    b.finish_extend()
                        .object()
                        .property("brightness", true, |b| {
                            b.finish_extend()
                                .integer()
                                .minimum(0)
                                .maximum(100)
                                .unit("percent")
                        })
                        .property("duration", true, |b| {
                            b.finish_extend().integer().minimum(1).unit("milliseconds")
                        })
                })
        })
        .event("overheated", |b| {
            b.description("The lamp has exceeded its safe operating temperature")
                .form(|b| {
                    b.href("/events/overheated")
                        .http_get(overheated_events)
                        .op(wot_td::thing::FormOperation::SubscribeEvent)
                        .op(wot_td::thing::FormOperation::UnsubscribeEvent)
                        .subprotocol("sse")
                })
                .data(|b| b.finish_extend().number().unit("degree celsius"))
        })
        .http_bind(addr)
        .build_servient()
        .expect("cannot build Thing Descriptor for the lamp");

    servient.router = servient.router.layer(Extension(app_state));

    let axum_future = async {
        tracing::debug!("listening on {}", addr);
        servient
            .serve()
            .await
            .unwrap_or_else(|err| panic!("unable to create web server on address {addr}: {err}"));
    };

    join!(
        handle_messages(lamp, message_sender, message_receiver, event_sender),
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
    async fn get_brightness(&self) -> u8 {
        self.use_oneshot(Message::GetBrightness).await
    }

    #[inline]
    async fn get_is_on(&self) -> bool {
        self.use_oneshot(Message::GetIsOn).await
    }

    #[inline]
    async fn set_brightness(&self, value: u8) {
        self.send_message(Message::SetBrightness(value)).await;
    }

    #[inline]
    async fn set_is_on(&self, value: bool) {
        self.send_message(Message::SetIsOn(value)).await;
    }

    #[inline]
    async fn get_actions(&self) -> Arc<Vec<StoredAction>> {
        self.use_oneshot(Message::GetActions).await
    }

    #[inline]
    async fn delete_action(&self, name: ActionName, id: Uuid) -> bool {
        self.use_oneshot(|sender| Message::DeleteAction {
            _name: name,
            _id: id,
            _sender: sender,
        })
        .await
    }

    #[inline]
    async fn fade(&self, data: FadeActionInput) -> StoredAction {
        self.use_oneshot(|sender| {
            let FadeActionInput {
                brightness,
                duration,
            } = data;
            let time_requested = OffsetDateTime::now_utc();
            let duration = Duration::from_millis(duration);
            Message::Fade {
                _brightness: brightness,
                _time_requested: time_requested,
                _duration: duration,
                _sender: sender,
            }
        })
        .await
    }
}

#[derive(Debug)]
enum Message {
    GetProperties(oneshot::Sender<Properties>),
    GetBrightness(oneshot::Sender<u8>),
    GetIsOn(oneshot::Sender<bool>),
    SetBrightness(u8),
    SetIsOn(bool),
    GetActions(oneshot::Sender<Arc<Vec<StoredAction>>>),
    DeleteAction {
        _sender: oneshot::Sender<bool>,
        _name: ActionName,
        _id: Uuid,
    },
    Fade {
        _brightness: u8,
        _time_requested: OffsetDateTime,
        _duration: Duration,
        _sender: oneshot::Sender<StoredAction>,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
enum Event {}

async fn handle_messages(
    _lamp: Lamp,
    _message_sender: mpsc::Sender<Message>,
    _receiver: mpsc::Receiver<Message>,
    _event_sender: broadcast::Sender<Event>,
) {
    todo!()
}

#[derive(Debug, Serialize, Deserialize)]
struct Properties {
    brightness: u8,
    on: bool,
}

async fn properties(Extension(state): Extension<AppState>) -> Json<Properties> {
    let properties = state.get_properties().await;

    Json(properties)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Property {
    Brightness(u8),
    On(bool),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PropertyName {
    Brightness,
    On,
}

fn handle_sse_stream<F>(
    Extension(state): Extension<AppState>,
    mut f: F,
) -> Sse<impl Stream<Item = Result<sse::Event, serde_json::Error>>>
where
    F: FnMut(Event) -> Result<Option<sse::Event>, serde_json::Error> + Send + 'static,
{
    let receiver = BroadcastStream::new(state.event_sender.subscribe())
        .filter_map(move |ev| ev.ok().and_then(|v| f(v).transpose()));

    Sse::new(receiver).keep_alive(KeepAlive::default())
}

fn handle_overheated_event(_event: Event) -> Result<Option<sse::Event>, serde_json::Error> {
    todo!()
}

#[inline]
async fn all_events(
    extension: Extension<AppState>,
) -> Sse<impl Stream<Item = Result<sse::Event, serde_json::Error>>> {
    handle_sse_stream(extension, handle_overheated_event)
}

#[inline]
async fn overheated_events(
    extension: Extension<AppState>,
) -> Sse<impl Stream<Item = Result<sse::Event, serde_json::Error>>> {
    handle_sse_stream(extension, handle_overheated_event)
}

async fn get_actions(Extension(state): Extension<AppState>) -> impl IntoResponse {
    let mut actions = HashMap::<_, Vec<_>>::new();
    for action in &*state.get_actions().await {
        actions
            .entry(action.ty.name())
            .or_default()
            .push(action.clone());
    }

    Json(actions)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FadeActionInput {
    brightness: u8,
    duration: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredAction {
    #[serde(skip)]
    _id: Uuid,
    #[serde(flatten)]
    ty: StoredActionType,
}

#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
enum ActionStatusStatus {
    #[default]
    Pending,
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", untagged)]
enum StoredActionType {
    Fade(ActionStatus<FadeActionInput>),
}

impl StoredActionType {
    fn name(&self) -> &'static str {
        match self {
            Self::Fade(_) => "fade",
        }
    }

    fn into_output_href(self) -> Option<(Option<FadeActionInput>, Option<String>)> {
        match self {
            Self::Fade(ActionStatus { output, href, .. }) => Some((output, href)),
        }
    }
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActionStatus<Output> {
    #[serde(default)]
    status: ActionStatusStatus,
    output: Option<Output>,
    error: Option<HttpApiProblem>,
    href: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    time_requested: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    time_ended: Option<OffsetDateTime>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum ActionName {
    Fade,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum EventName {
    Overheated,
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

async fn get_brightness_property(Extension(app): Extension<AppState>) -> Json<u8> {
    let brightness = app.get_brightness().await;
    Json(brightness)
}

async fn put_brightness_property(
    Json(value): Json<u8>,
    Extension(app): Extension<AppState>,
) -> impl IntoResponse {
    app.set_brightness(value).await;
    StatusCode::NO_CONTENT
}

fn handle_brightness_stream(_event: Event) -> Result<Option<sse::Event>, serde_json::Error> {
    todo!()
}

#[inline]
async fn observe_brightness(
    extension: Extension<AppState>,
) -> Sse<impl Stream<Item = Result<sse::Event, serde_json::Error>>> {
    handle_sse_stream(extension, handle_brightness_stream)
}

#[inline]
async fn observe_properties(
    extension: Extension<AppState>,
) -> Sse<impl Stream<Item = Result<sse::Event, serde_json::Error>>> {
    handle_sse_stream(extension, handle_brightness_stream)
}

async fn get_fade_action(
    Path(_id): Path<Uuid>,
    Extension(_app): Extension<AppState>,
) -> impl IntoResponse {
    todo!()
}

async fn post_fade_action(
    Json(input): Json<FadeActionInput>,
    Extension(state): Extension<AppState>,
) -> impl IntoResponse {
    let action = state.fade(input).await;
    let (output, href) = action.ty.into_output_href().unwrap();

    let response = ActionStatus {
        status: ActionStatusStatus::Pending,
        output: Some(output),
        error: None,
        href: href.clone(),
        time_requested: None,
        time_ended: None,
    };

    match href {
        Some(href) => (StatusCode::CREATED, [("location", href)], Json(response)).into_response(),
        None => (StatusCode::CREATED, Json(response)).into_response(),
    }
}

async fn delete_fade_action(
    Path(id): Path<Uuid>,
    Extension(state): Extension<AppState>,
) -> impl IntoResponse {
    match state.delete_action(ActionName::Fade, id).await {
        true => StatusCode::NO_CONTENT,
        false => StatusCode::NOT_FOUND,
    }
}
