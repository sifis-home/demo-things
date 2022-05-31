use bytes::Bytes;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::{
    join,
    sync::{mpsc, oneshot},
    time::sleep,
};
use uuid::Uuid;

use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{delete, get, post, put},
    Extension, Router,
};
use wot_td::{
    builder::data_schema::{
        BuildableDataSchema, DataSchemaBuilder, IntegerDataSchemaBuilderLike,
        ObjectDataSchemaBuilderLike, SpecializableDataSchema,
    },
    thing::{
        ActionAffordance, EventAffordance, Form, InteractionAffordance, PropertyAffordance, Thing,
    },
};

struct Lamp {
    is_on: bool,
    brightness: u8,
    actions: Vec<StoredAction>,
}

const MESSAGE_QUEUE_LENGTH: usize = 16;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt::init();

    let thing = Thing::build("My Lamp")
        .id("urn:dev:ops:my-lamp-1234")
        .attype("OnOffSwitch")
        .attype("Light")
        .description("A web connected lamp")
        .security(|b| b.no_sec().with_key("nosec_sc").required())
        .build()
        .expect("cannot build Thing Descriptor for the lamp");

    let thing = Thing {
        properties: Some(
            [
                (
                    "on".to_string(),
                    PropertyAffordance {
                        interaction: InteractionAffordance {
                            attype: Some(vec!["OnOffProperty".to_string()]),
                            title: Some("On/Off".to_string()),
                            titles: None,
                            description: Some("Whether the lamp is turned on".to_string()),
                            descriptions: None,
                            forms: vec![Form {
                                op: Default::default(),
                                href: "/properties/on".to_string(),
                                content_type: Default::default(),
                                content_coding: Default::default(),
                                subprotocol: Default::default(),
                                security: Default::default(),
                                scopes: Default::default(),
                                response: Default::default(),
                            }],
                            uri_variables: None,
                        },
                        data_schema: DataSchemaBuilder::default().bool().into(),
                        observable: None,
                    },
                ),
                (
                    "brightness".to_string(),
                    PropertyAffordance {
                        interaction: InteractionAffordance {
                            attype: Some(vec!["BrightnessProperty".to_string()]),
                            title: Some("Brightness".to_string()),
                            titles: None,
                            description: Some("The level of light from 0-100".to_string()),
                            descriptions: None,
                            forms: vec![Form {
                                op: Default::default(),
                                href: "/properties/brightness".to_string(),
                                content_type: Default::default(),
                                content_coding: Default::default(),
                                subprotocol: Default::default(),
                                security: Default::default(),
                                scopes: Default::default(),
                                response: Default::default(),
                            }],
                            uri_variables: None,
                        },
                        data_schema: DataSchemaBuilder::default()
                            .integer()
                            .minimum(0)
                            .maximum(100)
                            .unit("percent")
                            .into(),
                        observable: None,
                    },
                ),
            ]
            .into_iter()
            .collect(),
        ),
        actions: Some(
            [(
                "fade".to_string(),
                ActionAffordance {
                    interaction: InteractionAffordance {
                        attype: None,
                        title: Some("Fade".to_string()),
                        titles: None,
                        description: Some("Fade the lamp to a given level".to_string()),
                        descriptions: None,
                        forms: vec![Form {
                            op: Default::default(),
                            href: "/actions/fade".to_string(),
                            content_type: Default::default(),
                            content_coding: Default::default(),
                            subprotocol: Default::default(),
                            security: Default::default(),
                            scopes: Default::default(),
                            response: Default::default(),
                        }],
                        uri_variables: None,
                    },
                    input: Some(
                        DataSchemaBuilder::default()
                            .object()
                            .property("brightness", true, |b| {
                                b.integer().minimum(0).maximum(100).unit("percent")
                            })
                            .property("duration", true, |b| {
                                b.integer().minimum(1).unit("milliseconds")
                            })
                            .into(),
                    ),
                    output: None,
                    safe: false,
                    idempotent: false,
                },
            )]
            .into_iter()
            .collect(),
        ),
        events: Some(
            [(
                "overheated".to_string(),
                EventAffordance {
                    interaction: InteractionAffordance {
                        attype: None,
                        title: None,
                        titles: None,
                        description: Some(
                            "The lamp has exceeded its safe operating temperature".to_string(),
                        ),
                        descriptions: None,
                        forms: vec![Form {
                            op: Default::default(),
                            href: "/events/overheated".to_string(),
                            content_type: Default::default(),
                            content_coding: Default::default(),
                            subprotocol: Default::default(),
                            security: Default::default(),
                            scopes: Default::default(),
                            response: Default::default(),
                        }],
                        uri_variables: None,
                    },
                    subscription: None,
                    data: Some(
                        DataSchemaBuilder::default()
                            .number()
                            .unit("degree celsius")
                            .into(),
                    ),
                    cancellation: None,
                },
            )]
            .into_iter()
            .collect(),
        ),
        ..thing
    };

    let lamp = Lamp {
        is_on: true,
        brightness: 50,
        actions: Default::default(),
    };

    let (message_sender, message_receiver) = mpsc::channel(MESSAGE_QUEUE_LENGTH);

    let app_state = AppState {
        thing: Arc::new(thing),
        message_sender: message_sender.clone(),
    };

    let app = Router::new()
        .route("/", get(root))
        .route("/.well-known/wot-thing-description", get(root))
        .route("/properties", get(properties))
        .route("/properties/:property", get(get_property))
        .route("/properties/:property", put(put_property))
        .route("/actions", get(get_actions))
        .route("/actions", post(post_actions))
        .route("/actions/:action", get(get_action))
        .route("/actions/:action", post(post_action))
        .route("/actions/:action/:id", delete(delete_action))
        .route("/events", get(get_events))
        .route("/events/:event", get(get_event))
        .layer(Extension(app_state));

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    let axum_future = async {
        tracing::debug!("listening on {}", addr);
        axum::Server::bind(&addr)
            .serve(app.into_make_service())
            .await
            .unwrap_or_else(|err| panic!("unable to create web server on address {addr}: {err}"));
    };

    join!(
        handle_messages(lamp, message_sender, message_receiver),
        axum_future
    );
}

#[derive(Clone)]
struct AppState {
    thing: Arc<Thing>,
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
        self.use_oneshot(|sender| Message::DeleteAction { name, id, sender })
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
                brightness,
                time_requested,
                duration,
                sender,
            }
        })
        .await
    }

    #[inline]
    async fn get_events(&self) -> Arc<Vec<Event>> {
        self.use_oneshot(Message::GetEvents).await
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
        sender: oneshot::Sender<bool>,
        name: ActionName,
        id: Uuid,
    },
    Fade {
        brightness: u8,
        time_requested: OffsetDateTime,
        duration: Duration,
        sender: oneshot::Sender<StoredAction>,
    },
    CompleteAction(Uuid),
    GetEvents(oneshot::Sender<Arc<Vec<Event>>>),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
enum Event {
    Overheated {
        data: u16,
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
}

impl Event {
    fn to_overheated(&self) -> Option<&Self> {
        match self {
            Self::Overheated { .. } => Some(self),
        }
    }
}

async fn handle_messages(
    lamp: Lamp,
    message_sender: mpsc::Sender<Message>,
    mut receiver: mpsc::Receiver<Message>,
) {
    use Message::*;

    let Lamp {
        mut is_on,
        mut brightness,
        actions,
    } = lamp;
    let mut actions = Arc::new(actions);
    let mut events = Arc::new(Vec::new());

    while let Some(message) = receiver.recv().await {
        match message {
            GetProperties(sender) => {
                let properties = Properties {
                    brightness,
                    on: is_on,
                };
                sender.send(properties).unwrap();
            }
            GetBrightness(sender) => sender.send(brightness).unwrap(),
            GetIsOn(sender) => sender.send(is_on).unwrap(),
            SetBrightness(value) => brightness = value,
            SetIsOn(value) => is_on = value,
            GetActions(sender) => sender.send(Arc::clone(&actions)).unwrap(),
            DeleteAction { sender, name, id } => {
                let filter = match name {
                    ActionName::Fade => StoredAction::to_fade,
                };

                let index = actions.iter().position(|action| {
                    filter(action)
                        .map(|action| action.id == id)
                        .unwrap_or(false)
                });

                let removed = match index {
                    Some(index) => {
                        Arc::make_mut(&mut actions).swap_remove(index);
                        true
                    }
                    None => false,
                };
                sender.send(removed).unwrap()
            }
            Fade {
                brightness,
                duration,
                time_requested,
                sender,
            } => {
                let now = OffsetDateTime::now_utc();
                let real_duration = duration
                    .saturating_sub((now - time_requested).try_into().unwrap_or(Duration::ZERO));
                let id = Uuid::new_v4();
                let href = format!("/actions/fade/{}", id);
                let fade_action = StoredFadeAction {
                    input: FadeActionInput {
                        brightness,
                        duration: duration.as_millis().try_into().unwrap_or(u64::MAX),
                    },
                    href,
                    time_requested,
                    time_completed: None,
                    status: ActionStatus::Pending,
                };

                Arc::make_mut(&mut actions).push(StoredAction {
                    id,
                    ty: StoredActionType::Fade(StoredFadeAction {
                        status: ActionStatus::Created,
                        ..fade_action.clone()
                    }),
                });
                Arc::make_mut(&mut events).push(Event::Overheated {
                    data: 102,
                    timestamp: now,
                });
                let message_sender = message_sender.clone();
                tokio::spawn(async move {
                    sleep(real_duration).await;
                    if let Err(err) = message_sender.send(CompleteAction(id)).await {
                        tracing::warn!("unable to queue complete action: {err}");
                    }
                });

                let action = StoredAction {
                    id,
                    ty: StoredActionType::Fade(fade_action),
                };
                sender.send(action).unwrap();
            }
            CompleteAction(id) => {
                let actions = Arc::make_mut(&mut actions);
                let action = match actions.iter_mut().find(|action| action.id == id) {
                    Some(action) => action,
                    None => {
                        tracing::warn!("unable to complete action with id {id}");
                        continue;
                    }
                };

                match &mut action.ty {
                    StoredActionType::Fade(action) => {
                        let now = OffsetDateTime::now_utc();
                        brightness = action.input.brightness;
                        action.time_completed = Some(now);
                        action.status = ActionStatus::Completed;
                    }
                }
            }
            GetEvents(sender) => sender.send(Arc::clone(&events)).unwrap(),
        }
    }
}

async fn root(Extension(state): Extension<AppState>) -> impl IntoResponse {
    static RESPONSE: OnceCell<Bytes> = OnceCell::new();

    let response = RESPONSE.get_or_init(|| {
        serde_json::to_vec(&state.thing)
            .expect("unable to convert Thing description to JSON")
            .into()
    });

    (
        [(header::CONTENT_TYPE, "application/json")],
        response.clone(),
    )
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

async fn get_property(
    Path(property): Path<PropertyName>,
    Extension(state): Extension<AppState>,
) -> Json<Value> {
    let property: Value = match property {
        PropertyName::Brightness => state.get_brightness().await.into(),
        PropertyName::On => state.get_is_on().await.into(),
    };

    Json(property)
}

enum AppError {
    InvalidValue,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            Self::InvalidValue => (StatusCode::BAD_REQUEST, "Invalid value"),
        };

        let body = Json(json!({
            "error": error_message,
        }));

        (status, body).into_response()
    }
}

async fn put_property(
    Path(property): Path<PropertyName>,
    Json(value): Json<Value>,
    Extension(state): Extension<AppState>,
) -> Result<Json<Value>, AppError> {
    let property: Value = match (property, value) {
        (PropertyName::Brightness, Value::Number(value)) => {
            let value = value
                .as_u64()
                .and_then(|value| value.try_into().ok())
                .ok_or(AppError::InvalidValue)?;
            state.set_brightness(value).await;
            value.into()
        }
        (PropertyName::On, Value::Bool(value)) => {
            state.set_is_on(value).await;
            value.into()
        }
        _ => return Err(AppError::InvalidValue),
    };

    Ok(Json(property))
}

async fn get_events(Extension(state): Extension<AppState>) -> impl IntoResponse {
    let bytes: Bytes = serde_json::to_vec(&*state.get_events().await)
        .expect("unable to convert Thing events to JSON")
        .into();

    ([(header::CONTENT_TYPE, "application/json")], bytes)
}

async fn get_actions(Extension(state): Extension<AppState>) -> impl IntoResponse {
    let bytes: Bytes = serde_json::to_vec(&*state.get_actions().await)
        .expect("unable to convert Thing actions to JSON")
        .into();

    ([(header::CONTENT_TYPE, "application/json")], bytes)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Action {
    Fade(FadeAction),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FadeAction {
    input: FadeActionInput,
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
    id: Uuid,
    #[serde(flatten)]
    ty: StoredActionType,
}

impl StoredAction {
    fn to_fade(&self) -> Option<&Self> {
        match &self.ty {
            StoredActionType::Fade(_) => Some(self),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
enum ActionStatus {
    Created,
    Pending,
    Completed,
}

impl Default for ActionStatus {
    fn default() -> Self {
        Self::Created
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum StoredActionType {
    Fade(StoredFadeAction),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredFadeAction {
    input: FadeActionInput,
    href: String,
    #[serde(with = "time::serde::rfc3339")]
    time_requested: OffsetDateTime,
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    time_completed: Option<OffsetDateTime>,
    #[serde(default)]
    status: ActionStatus,
}

async fn post_actions(
    Json(value): Json<Value>,
    Extension(state): Extension<AppState>,
) -> Result<impl IntoResponse, AppError> {
    let action: Action = serde_json::from_value(value).map_err(|_| AppError::InvalidValue)?;
    match action {
        Action::Fade(fade) => {
            let action = state.fade(fade.input).await;
            let (input, href) = match action.ty {
                StoredActionType::Fade(StoredFadeAction { input, href, .. }) => (input, href),
            };

            let response = (
                StatusCode::CREATED,
                Json(json!({
                    "fade": {
                        "input": input,
                        "href": href,
                        "status": ActionStatus::Created,
                    }
                })),
            );
            Ok(response)
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum ActionName {
    Fade,
}

async fn get_action(
    Path(name): Path<ActionName>,
    Extension(state): Extension<AppState>,
) -> Json<Vec<StoredAction>> {
    let filter = match name {
        ActionName::Fade => StoredAction::to_fade,
    };

    Json(
        state
            .get_actions()
            .await
            .iter()
            .filter_map(filter)
            .cloned()
            .collect(),
    )
}

async fn delete_action(
    Path((name, id)): Path<(ActionName, Uuid)>,
    Extension(state): Extension<AppState>,
) -> impl IntoResponse {
    match state.delete_action(name, id).await {
        true => StatusCode::NO_CONTENT,
        false => StatusCode::NOT_FOUND,
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum EventName {
    Overheated,
}

async fn get_event(
    Path(name): Path<EventName>,
    Extension(state): Extension<AppState>,
) -> Json<Vec<Event>> {
    let filter = match name {
        EventName::Overheated => Event::to_overheated,
    };

    Json(
        state
            .get_events()
            .await
            .iter()
            .filter_map(filter)
            .cloned()
            .collect(),
    )
}

async fn post_action(
    Path(action_name): Path<ActionName>,
    Json(value): Json<Value>,
    Extension(state): Extension<AppState>,
) -> Result<impl IntoResponse, AppError> {
    let action: Action = serde_json::from_value(value).map_err(|_| AppError::InvalidValue)?;
    match (action_name, action) {
        (ActionName::Fade, Action::Fade(fade)) => {
            let action = state.fade(fade.input).await;
            let (input, href) = match action.ty {
                StoredActionType::Fade(StoredFadeAction { input, href, .. }) => (input, href),
            };

            let response = (
                StatusCode::CREATED,
                Json(json!({
                    "fade": {
                        "input": input,
                        "href": href,
                        "status": ActionStatus::Created,
                    }
                })),
            );
            Ok(response)
        }
    }
}
