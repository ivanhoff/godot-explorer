use godot::prelude::*;
use http::Uri;

use crate::{
    auth::dcl_player_identity::DclPlayerIdentity,
    comms::{
        adapter::{livekit::LivekitRoom, ws_room::WebSocketRoom},
        signed_login::SignedLoginMeta,
    },
    dcl::components::proto_components::kernel::comms::rfc4,
    godot_classes::dcl_global::DclGlobal,
};

use super::{
    adapter::adapter_trait::Adapter,
    signed_login::{SignedLogin, SignedLoginPollStatus},
};

#[allow(clippy::large_enum_variant)]
enum CommsConnection {
    None,
    WaitingForIdentity(String),
    SignedLogin(SignedLogin),
    Connected(Box<dyn Adapter>),
}

#[derive(GodotClass)]
#[class(base=Node)]
pub struct CommunicationManager {
    current_connection: CommsConnection,
    current_connection_str: String,
    last_position_broadcast_index: u64,

    #[var]
    player_identity: Gd<DclPlayerIdentity>,

    #[base]
    base: Base<Node>,
}

#[godot_api]
impl NodeVirtual for CommunicationManager {
    fn init(base: Base<Node>) -> Self {
        CommunicationManager {
            current_connection: CommsConnection::None,
            current_connection_str: String::default(),
            last_position_broadcast_index: 0,
            player_identity: Gd::new_default(),
            base,
        }
    }

    fn ready(&mut self) {
        self.base.call_deferred("init_rs".into(), &[]);
        self.base.add_child(self.player_identity.clone().upcast());
    }

    fn process(&mut self, _dt: f64) {
        match &mut self.current_connection {
            CommsConnection::None => {}
            CommsConnection::WaitingForIdentity(adapter_url) => {
                if self.player_identity.bind().is_connected() {
                    self.base
                        .call_deferred("change_adapter".into(), &[adapter_url.to_variant()]);
                }
            }
            CommsConnection::SignedLogin(signed_login) => match signed_login.poll() {
                SignedLoginPollStatus::Pending => {}
                SignedLoginPollStatus::Complete(response) => {
                    self.change_adapter(response.fixed_adapter.unwrap_or("offline".into()).into());
                }
                SignedLoginPollStatus::Error(e) => {
                    tracing::info!("Error in signed login: {:?}", e);
                    self.current_connection = CommsConnection::None;
                }
            },
            CommsConnection::Connected(adapter) => {
                let adapter = adapter.as_mut();
                let adapter_polling_ok = adapter.poll();
                let chats = adapter.consume_chats();

                if !chats.is_empty() {
                    let mut chats_variant_array = VariantArray::new();
                    for (address, profile_name, chat) in chats {
                        let mut chat_arr = VariantArray::new();
                        chat_arr.push(address.to_variant());
                        chat_arr.push(profile_name.to_variant());
                        chat_arr.push(chat.timestamp.to_variant());
                        chat_arr.push(chat.message.to_variant());

                        chats_variant_array.push(chat_arr.to_variant());
                    }
                    self.base
                        .emit_signal("chat_message".into(), &[chats_variant_array.to_variant()]);
                }

                if !adapter_polling_ok {
                    self.current_connection = CommsConnection::None;
                }
            }
        }
    }
}

impl CommunicationManager {}

#[godot_api]
impl CommunicationManager {
    #[signal]
    fn chat_message(chats: VariantArray) {}

    #[func]
    fn broadcast_voice(&mut self, frame: PackedVector2Array) {
        let CommsConnection::Connected(adapter) = &mut self.current_connection else {
            return;
        };
        if !adapter.support_voice_chat() {
            return;
        }

        let mut max_value = 0;
        let vec = frame
            .as_slice()
            .iter()
            .map(|v| {
                let value = ((0.5 * (v.x + v.y)) * i16::MAX as f32) as i16;

                max_value = std::cmp::max(max_value, value);
                value
            })
            .collect::<Vec<i16>>();

        if max_value > 0 {
            adapter.broadcast_voice(vec);
        }
    }

    #[func]
    fn broadcast_position_and_rotation(&mut self, position: Vector3, rotation: Quaternion) -> bool {
        let index = self.last_position_broadcast_index;
        let get_packet = || {
            let position_packet = rfc4::Position {
                index: index as u32,
                position_x: position.x,
                position_y: position.y,
                position_z: -position.z,
                rotation_x: rotation.x,
                rotation_y: rotation.y,
                rotation_z: -rotation.z,
                rotation_w: -rotation.w,
            };

            rfc4::Packet {
                message: Some(rfc4::packet::Message::Position(position_packet)),
            }
        };

        let message_sent = match &mut self.current_connection {
            CommsConnection::None
            | CommsConnection::SignedLogin(_)
            | CommsConnection::WaitingForIdentity(_) => false,
            CommsConnection::Connected(adapter) => adapter.send_rfc4(get_packet(), true),
        };

        if message_sent {
            self.last_position_broadcast_index += 1;
        }
        message_sent
    }

    #[func]
    fn send_chat(&mut self, text: GodotString) -> bool {
        let get_packet = || rfc4::Packet {
            message: Some(rfc4::packet::Message::Chat(rfc4::Chat {
                message: text.to_string(),
                timestamp: 0.0,
            })),
        };

        match &mut self.current_connection {
            CommsConnection::None
            | CommsConnection::SignedLogin(_)
            | CommsConnection::WaitingForIdentity(_) => false,
            CommsConnection::Connected(adapter) => adapter.send_rfc4(get_packet(), false),
        }
    }

    #[func]
    fn init_rs(&mut self) {
        let on_realm_changed =
            Callable::from_object_method(self.base.clone(), StringName::from("_on_realm_changed"));

        DclGlobal::singleton()
            .bind()
            .get_realm()
            .connect("realm_changed".into(), on_realm_changed);
    }

    #[func]
    fn _on_realm_changed(&mut self) {
        self.base
            .call_deferred("_on_realm_changed_deferred".into(), &[]);
    }

    fn _internal_get_comms_from_realm(&self) -> Option<(String, Option<GodotString>)> {
        let realm = DclGlobal::singleton().bind().get_realm();
        let realm_about = Dictionary::from_variant(&realm.get("realm_about".into()));
        let comms = Dictionary::from_variant(&realm_about.get(StringName::from("comms"))?);
        let comms_protocol = String::from_variant(&comms.get(StringName::from("protocol"))?);
        let comms_fixed_adapter = comms
            .get(StringName::from("fixedAdapter"))
            .map(|v| GodotString::from_variant(&v));

        Some((comms_protocol, comms_fixed_adapter))
    }

    #[func]
    fn _on_realm_changed_deferred(&mut self) {
        self.clean();

        let comms = self._internal_get_comms_from_realm();
        if comms.is_none() {
            tracing::info!("invalid comms from realm.");
            return;
        }

        let (comms_protocol, comms_fixed_adapter) = comms.unwrap();
        if comms_protocol != "v3" {
            tracing::info!("Only protocol 'v3' is supported.");
            return;
        }

        if comms_fixed_adapter.is_none() {
            tracing::info!("As far, only fixedAdapter is supported.");
            return;
        }

        let comms_fixed_adapter_str = comms_fixed_adapter.unwrap().to_string();
        self.change_adapter(comms_fixed_adapter_str.into());
    }

    #[func]
    fn change_adapter(&mut self, comms_fixed_adapter_str: GodotString) {
        let comms_fixed_adapter_str = comms_fixed_adapter_str.to_string();
        let Some((protocol, comms_address)) = comms_fixed_adapter_str.as_str().split_once(':')
        else {
            tracing::warn!("unrecognised fixed adapter string: {comms_fixed_adapter_str}");
            return;
        };

        if !self.player_identity.bind().is_connected() {
            self.current_connection = CommsConnection::WaitingForIdentity(comms_fixed_adapter_str);
            return;
        }

        self.current_connection = CommsConnection::None;
        self.current_connection_str = comms_fixed_adapter_str.clone();
        let avatar_scene = DclGlobal::singleton().bind().get_avatars();

        tracing::info!("change_adapter to protocol {protocol} and address {comms_address}");

        let current_ephemeral_auth_chain = self
            .player_identity
            .bind()
            .try_get_ephemeral_auth_chain()
            .expect("ephemeral auth chain needed to start a comms connection");

        let player_profile = self.player_identity.bind().profile().clone();

        match protocol {
            "ws-room" => {
                self.current_connection = CommsConnection::Connected(Box::new(WebSocketRoom::new(
                    comms_address,
                    current_ephemeral_auth_chain,
                    player_profile,
                    avatar_scene,
                )));
            }
            "signed-login" => {
                let Ok(uri) = Uri::try_from(comms_address.to_string()) else {
                    tracing::warn!(
                        "failed to parse signed login comms_address as a uri: {comms_address}"
                    );
                    return;
                };

                let realm_url = DclGlobal::singleton()
                    .bind()
                    .get_realm()
                    .get("realm_url".into())
                    .to_string();
                let Ok(origin) = Uri::try_from(&realm_url) else {
                    tracing::warn!("failed to parse origin comms_address as a uri: {realm_url}");
                    return;
                };

                self.current_connection = CommsConnection::SignedLogin(SignedLogin::new(
                    uri,
                    current_ephemeral_auth_chain,
                    SignedLoginMeta::new(true, origin),
                ));
            }
            "livekit" => {
                self.current_connection = CommsConnection::Connected(Box::new(LivekitRoom::new(
                    comms_address.to_string(),
                    current_ephemeral_auth_chain.signer(),
                    player_profile,
                    avatar_scene,
                )));
            }
            "offline" => {
                tracing::info!("set offline");
            }
            _ => {
                tracing::info!("unknown adapter {:?}", protocol);
            }
        }
    }

    fn clean(&mut self) {
        match &mut self.current_connection {
            CommsConnection::None
            | CommsConnection::SignedLogin(_)
            | CommsConnection::WaitingForIdentity(_) => {}
            CommsConnection::Connected(adapter) => {
                adapter.clean();
            }
        }

        self.current_connection = CommsConnection::None;
        self.current_connection_str = String::default();
    }

    #[func]
    fn _on_player_identity_profile_changed(&mut self, _new_profile: Dictionary) {
        match &mut self.current_connection {
            CommsConnection::None
            | CommsConnection::SignedLogin(_)
            | CommsConnection::WaitingForIdentity(_) => {}
            CommsConnection::Connected(adapter) => {
                let player_profile = self.player_identity.bind().profile().clone();
                adapter.change_profile(player_profile);
            }
        }
    }

    #[func]
    fn disconnect(&mut self, sign_out_session: bool) {
        self.clean();
        if sign_out_session {
            self.player_identity.bind_mut().logout();
        }
    }

    #[func]
    pub fn get_current_adapter_conn_str(&self) -> GodotString {
        GodotString::from(self.current_connection_str.clone())
    }
}
