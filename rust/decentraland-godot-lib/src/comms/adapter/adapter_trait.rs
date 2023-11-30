use crate::{comms::profile::UserProfile, dcl::components::proto_components::kernel::comms::rfc4};

pub trait Adapter {
    fn poll(&mut self) -> bool;
    fn clean(&mut self);

    fn consume_chats(&mut self) -> Vec<(String, String, rfc4::Chat)>;
    fn change_profile(&mut self, new_profile: UserProfile);

    fn send_rfc4(&mut self, packet: rfc4::Packet, unreliable: bool) -> bool;

    fn broadcast_voice(&mut self, frame: Vec<i16>);
    fn support_voice_chat(&self) -> bool;
}