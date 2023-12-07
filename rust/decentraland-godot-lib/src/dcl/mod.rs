pub mod components;
pub mod crdt;
pub mod js;
pub mod scene_apis;
pub mod serialization;

use godot::builtin::{Vector2, Vector3};

use crate::auth::wallet::Wallet;

use self::{
    crdt::{DirtyCrdtState, SceneCrdtState},
    js::{
        scene_thread,
        testing::{TakeAndCompareSnapshotResponse, TestingScreenshotComparisonMethodRequest},
        SceneLogMessage,
    },
    scene_apis::{RpcCall, RpcResultSender},
};

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

#[derive(Default, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy, Debug)]
pub struct SceneId(pub i32);

impl SceneId {
    pub const INVALID: SceneId = SceneId(-1);
}

// scene metadata
#[derive(Clone, Default, Debug)]
pub struct SceneDefinition {
    pub entity_id: String,
    pub path: String,
    pub main_crdt_path: String,
    pub base: godot::prelude::Vector2i,
    pub visible: bool,
    pub title: String,

    pub parcels: Vec<godot::prelude::Vector2i>,
    pub is_global: bool,
    pub metadata: String,
}
// data from renderer to scene
#[derive(Debug)]
pub enum RendererResponse {
    Ok(DirtyCrdtState),
    Kill,
}

// data from scene to renderer
#[derive(Debug)]
pub enum SceneResponse {
    Error(SceneId, String),
    Ok(
        SceneId,
        DirtyCrdtState,
        Vec<SceneLogMessage>,
        f32,
        Vec<RpcCall>,
    ),
    RemoveGodotScene(SceneId, Vec<SceneLogMessage>),
    TakeSnapshot {
        scene_id: SceneId,
        src_stored_snapshot: String,
        camera_position: Vector3,
        camera_target: Vector3,
        screeshot_size: Vector2,
        method: TestingScreenshotComparisonMethodRequest,
        response: RpcResultSender<Result<TakeAndCompareSnapshotResponse, String>>,
    },
}

pub type SharedSceneCrdtState = Arc<Mutex<SceneCrdtState>>;

pub struct DclScene {
    pub scene_id: SceneId,
    pub scene_crdt: SharedSceneCrdtState,
    pub main_sender_to_thread: tokio::sync::mpsc::Sender<RendererResponse>,
    pub thread_join_handle: JoinHandle<()>,
}

impl DclScene {
    pub fn spawn_new_js_dcl_scene(
        id: SceneId,
        scene_definition: SceneDefinition,
        content_mapping: HashMap<String, String>,
        base_url: String,
        thread_sender_to_main: std::sync::mpsc::SyncSender<SceneResponse>,
        wallet: Wallet,
        testing_mode: bool,
    ) -> Self {
        let (main_sender_to_thread, thread_receive_from_renderer) =
            tokio::sync::mpsc::channel::<RendererResponse>(1);

        let scene_crdt = Arc::new(Mutex::new(SceneCrdtState::from_proto()));
        let thread_scene_crdt = scene_crdt.clone();

        let thread_join_handle = std::thread::Builder::new()
            .name(format!("scene thread {}", id.0))
            .spawn(move || {
                scene_thread(
                    id,
                    scene_definition,
                    content_mapping,
                    base_url,
                    thread_sender_to_main,
                    thread_receive_from_renderer,
                    thread_scene_crdt,
                    wallet,
                    testing_mode,
                )
            })
            .unwrap();

        DclScene {
            scene_id: id,
            scene_crdt,
            main_sender_to_thread,
            thread_join_handle,
        }
    }

    pub fn spawn_new_test_scene(id: SceneId) -> Self {
        let (main_sender_to_thread, _thread_receive_from_renderer) =
            tokio::sync::mpsc::channel::<RendererResponse>(1);

        let scene_crdt = Arc::new(Mutex::new(SceneCrdtState::from_proto()));

        let thread_join_handle = std::thread::Builder::new()
            .name(format!("scene thread {}", id.0))
            .spawn(move || {})
            .unwrap();

        DclScene {
            scene_id: id,
            scene_crdt,
            main_sender_to_thread,
            thread_join_handle,
        }
    }
}
