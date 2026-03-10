// Copyright 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Element Call specific widget helpers.

use ruma::{
    events::call::member::{
        ActiveFocus, ActiveLivekitFocus, Application, CallApplicationContent,
        CallMemberEventContent, CallMemberStateKey, CallScope, Focus, LivekitFocus,
    },
    events::{MessageLikeEventType, StateEventType},
    DeviceId, RoomId, UserId,
};
use serde::Serialize;
use serde_json::{json, Value as JsonValue};

use super::{Capabilities, Filter, MessageLikeEventFilter, StateEventFilter, ToDeviceEventFilter};

/// Build the default Element Call capability set used by SDK-driven widgets.
pub fn element_call_capabilities(own_user_id: &UserId, own_device_id: &DeviceId) -> Capabilities {
    let read_send = vec![
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::from(
            "org.matrix.rageshake_request",
        ))),
        Filter::ToDevice(ToDeviceEventFilter::new("io.element.call.encryption_keys".into())),
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::from(
            "io.element.call.encryption_keys",
        ))),
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::from(
            "io.element.call.reaction",
        ))),
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::Reaction)),
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::RoomRedaction)),
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::RtcDecline)),
    ];

    let user_id = own_user_id.as_str();
    let device_id = own_device_id.as_str();
    let membership_state_key = CallMemberStateKey::new(
        own_user_id.to_owned(),
        Some(format!("{own_device_id}_m.call")),
        false,
    )
    .as_ref()
    .to_owned();

    Capabilities {
        read: vec![
            Filter::State(StateEventFilter::WithType(StateEventType::CallMember)),
            Filter::State(StateEventFilter::WithType(StateEventType::RoomName)),
            Filter::State(StateEventFilter::WithType(StateEventType::RoomMember)),
            Filter::State(StateEventFilter::WithType(StateEventType::RoomEncryption)),
            Filter::State(StateEventFilter::WithType(StateEventType::RoomCreate)),
        ]
        .into_iter()
        .chain(read_send.clone())
        .collect(),
        send: vec![
            Filter::MessageLike(MessageLikeEventFilter::WithType(
                MessageLikeEventType::RtcNotification,
            )),
            Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::CallNotify)),
            Filter::State(StateEventFilter::WithTypeAndStateKey(
                StateEventType::CallMember,
                user_id.to_owned(),
            )),
            Filter::State(StateEventFilter::WithTypeAndStateKey(
                StateEventType::CallMember,
                format!("{user_id}_{device_id}"),
            )),
            Filter::State(StateEventFilter::WithTypeAndStateKey(
                StateEventType::CallMember,
                membership_state_key,
            )),
            Filter::State(StateEventFilter::WithTypeAndStateKey(
                StateEventType::CallMember,
                format!("{user_id}_{device_id}_m.call"),
            )),
            Filter::State(StateEventFilter::WithTypeAndStateKey(
                StateEventType::CallMember,
                format!("_{user_id}_{device_id}"),
            )),
            Filter::State(StateEventFilter::WithTypeAndStateKey(
                StateEventType::CallMember,
                format!("_{user_id}_{device_id}_m.call"),
            )),
        ]
        .into_iter()
        .chain(read_send)
        .collect(),
        requires_client: true,
        update_delayed_event: true,
        send_delayed_event: true,
    }
}

/// Build `m.call.member` content suitable for publishing through the widget API.
pub fn element_call_member_content(
    room_id: &RoomId,
    own_device_id: &DeviceId,
    service_url: &str,
) -> CallMemberEventContent {
    let application =
        Application::Call(CallApplicationContent::new("".to_owned(), CallScope::Room));
    let focus_active = ActiveFocus::Livekit(ActiveLivekitFocus::new());
    let foci_preferred =
        vec![Focus::Livekit(LivekitFocus::new(room_id.to_string(), service_url.to_owned()))];

    CallMemberEventContent::new(
        application,
        own_device_id.to_owned(),
        focus_active,
        foci_preferred,
        None,
        None,
    )
}

/// Build a `fromWidget` `send_event` request payload for `m.call.member`.
pub fn element_call_send_event_message(
    widget_id: &str,
    request_id: &str,
    state_key: &str,
    content: &impl Serialize,
) -> JsonValue {
    json!({
        "api": "fromWidget",
        "widgetId": widget_id,
        "requestId": request_id,
        "action": "send_event",
        "data": {
            "type": "org.matrix.msc3401.call.member",
            "state_key": state_key,
            "content": content,
        },
    })
}
