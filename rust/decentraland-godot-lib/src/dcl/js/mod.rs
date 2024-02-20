mod comms;
mod engine;
mod ethereum_controller;
mod events;
mod fetch;
mod players;
mod portables;
mod restricted_actions;
mod runtime;
mod testing;
mod websocket;

use crate::auth::ephemeral_auth_chain::EphemeralAuthChain;
use crate::auth::ethereum_provider::EthereumProvider;
use crate::content::content_mapping::ContentMappingAndUrlRef;
use crate::dcl::common::{
    is_scene_log_enabled, SceneDying, SceneElapsedTime, SceneJsFileContent, SceneLogLevel,
    SceneLogMessage, SceneLogs, SceneMainCrdtFileContent, SceneStartTime,
};
use crate::dcl::scene_apis::{LocalCall, RpcCall};
use crate::realm::scene_definition::SceneEntityDefinition;

use super::{
    crdt::message::process_many_messages, serialization::reader::DclReader, SharedSceneCrdtState,
};
use super::{RendererResponse, SceneId, SceneResponse};

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use deno_core::error::JsError;
use deno_core::{
    ascii_str,
    error::{generic_error, AnyError},
    include_js_files, op, v8, Extension, Op, OpState, RuntimeOptions,
};
use once_cell::sync::Lazy;
use serde::Serialize;
use v8::IsolateHandle;

pub(crate) static VM_HANDLES: Lazy<std::sync::Mutex<HashMap<SceneId, IsolateHandle>>> =
    Lazy::new(Default::default);

pub fn create_runtime() -> deno_core::JsRuntime {
    let mut ext = &mut Extension::builder_with_deps("decentraland", &[]);

    // add core ops
    ext = ext.ops(vec![op_require::DECL, op_log::DECL, op_error::DECL]);

    let op_sets: [Vec<deno_core::OpDecl>; 11] = [
        engine::ops(),
        runtime::ops(),
        fetch::ops(),
        websocket::ops(),
        restricted_actions::ops(),
        portables::ops(),
        players::ops(),
        events::ops(),
        testing::ops(),
        ethereum_controller::ops(),
        comms::ops(),
    ];

    let mut op_map = HashMap::new();
    for set in op_sets {
        for op in &set {
            op_map.insert(op.name, *op);
        }
        ext = ext.ops(set)
    }

    let ext = ext
        // set startup JS script
        .esm(include_js_files!(
            GodotExplorer
            dir "src/dcl/js/js_modules",
            "main.js",
        ))
        .esm_entry_point("ext:GodotExplorer/main.js")
        .middleware(move |op| {
            if let Some(custom_op) = op_map.get(&op.name) {
                tracing::debug!("replace: {}", op.name);
                op.with_implementation_from(custom_op)
            } else {
                op
            }
        })
        .build();

    // create runtime
    deno_core::JsRuntime::new(RuntimeOptions {
        v8_platform: v8::Platform::new(1, false).make_shared().into(),
        extensions: vec![ext],
        ..Default::default()
    })
}

// main scene processing thread - constructs an isolate and runs the scene
#[allow(clippy::too_many_arguments)]
pub(crate) fn scene_thread(
    scene_id: SceneId,
    scene_entity_definition: Arc<SceneEntityDefinition>,
    local_main_js_file_path: String,
    local_main_crdt_file_path: String,
    content_mapping: ContentMappingAndUrlRef,
    thread_sender_to_main: std::sync::mpsc::SyncSender<SceneResponse>,
    thread_receive_from_main: tokio::sync::mpsc::Receiver<RendererResponse>,
    scene_crdt: SharedSceneCrdtState,
    testing_mode: bool,
    ethereum_provider: Arc<EthereumProvider>,
    ephemeral_wallet: Option<EphemeralAuthChain>,
) {
    let mut scene_main_crdt = None;

    // on main.crdt detected
    if !local_main_crdt_file_path.is_empty() {
        let file = godot::engine::FileAccess::open(
            godot::prelude::GString::from(local_main_crdt_file_path),
            godot::engine::file_access::ModeFlags::READ,
        );

        if let Some(file) = file {
            let buf = file.get_buffer(file.get_length() as i64).to_vec();

            let mut stream = DclReader::new(&buf);
            let mut scene_crdt_state = scene_crdt.lock().unwrap();

            process_many_messages(&mut stream, &mut scene_crdt_state);

            let dirty = scene_crdt_state.take_dirty();
            thread_sender_to_main
                .send(SceneResponse::Ok {
                    scene_id,
                    dirty_crdt_state: dirty,
                    logs: Vec::new(),
                    delta: 0.0,
                    rpc_calls: Vec::new(),
                })
                .expect("error sending scene response!!");

            scene_main_crdt = Some(buf);
        }
    }

    let file = godot::engine::FileAccess::open(
        godot::prelude::GString::from(local_main_js_file_path.clone()),
        godot::engine::file_access::ModeFlags::READ,
    );

    if file.is_none() {
        let err_string = format!("Scene `{local_main_js_file_path}` not found - file is none");
        if let Err(send_err) =
            thread_sender_to_main.send(SceneResponse::Error(scene_id, format!("{err_string:?}")))
        {
            tracing::info!("error sending error: {send_err:?}. original error {err_string:?}")
        }
        return;
    }
    let scene_code = SceneJsFileContent(file.unwrap().get_as_text().to_string());

    let mut runtime = create_runtime();

    // store handle
    let vm_handle = runtime.v8_isolate().thread_safe_handle();
    let mut guard = VM_HANDLES.lock().unwrap();
    guard.insert(scene_id, vm_handle);
    drop(guard);

    let state = runtime.op_state();

    state.borrow_mut().put(scene_code);

    state.borrow_mut().put(thread_sender_to_main);
    state.borrow_mut().put(thread_receive_from_main);
    state.borrow_mut().put(ethereum_provider);

    state.borrow_mut().put(scene_id);
    state.borrow_mut().put(scene_crdt);

    state.borrow_mut().put(ephemeral_wallet);
    state.borrow_mut().put(scene_entity_definition);

    state.borrow_mut().put(Vec::<RpcCall>::new());
    state.borrow_mut().put(Vec::<LocalCall>::new());

    // TODO: receive from main thread, and managed by command line params
    state.borrow_mut().put(SceneEnv {
        enable_know_env: testing_mode,
        testing_enable: testing_mode,
    });

    if let Some(scene_main_crdt) = scene_main_crdt {
        state
            .borrow_mut()
            .put(SceneMainCrdtFileContent(scene_main_crdt));
    }

    state.borrow_mut().put(content_mapping);

    state.borrow_mut().put(SceneLogs(Vec::new()));
    state.borrow_mut().put(SceneElapsedTime(0.0));
    state.borrow_mut().put(SceneDying(false));
    state
        .borrow_mut()
        .put(SceneStartTime(std::time::SystemTime::now()));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .enable_io()
        .build()
        .unwrap();

    let script = rt.block_on(async {
        runtime.execute_script(
            "<loader>",
            ascii_str!("const env = require('env');globalThis.DEBUG=true;require (\"~scene.js\")"),
        )
    });

    let script = match script {
        Err(e) => {
            tracing::error!("[scene thread {scene_id:?}] script load error: {}", e);
            return;
        }
        Ok(script) => script,
    };

    let utils_script = rt.block_on(async {
        runtime.execute_script("<loader>", ascii_str!("require (\"~utils.js\")"))
    });
    let utils_script = match utils_script {
        Err(e) => {
            tracing::error!("[scene thread {scene_id:?}] utils script load error: {}", e);
            return;
        }
        Ok(script) => script,
    };

    let result =
        rt.block_on(async { run_script(&mut runtime, &script, "onStart", |_| Vec::new()).await });
    if let Err(e) = result {
        tracing::error!("[scene thread {scene_id:?}] script load running: {}", e);
        return;
    }

    // instead of using run_event_loop for polling, this is a workaround to resolve pending promises
    let result = rt.block_on(async {
        run_script(&mut runtime, &utils_script, "run_async", |_| Vec::new()).await
    });
    if let Err(e) = result {
        tracing::error!("[scene thread {scene_id:?}] script load running: {}", e);
        return;
    }

    let start_time = std::time::SystemTime::now();
    let mut elapsed = Duration::default();

    loop {
        let dt = std::time::SystemTime::now()
            .duration_since(start_time)
            .unwrap_or(elapsed)
            - elapsed;
        elapsed += dt;

        state
            .borrow_mut()
            .put(SceneElapsedTime(elapsed.as_secs_f32()));

        // run the onUpdate function
        let result = rt.block_on(async {
            run_script(&mut runtime, &script, "onUpdate", |scope| {
                vec![v8::Number::new(scope, dt.as_secs_f64()).into()]
            })
            .await
        });

        if let Err(e) = result {
            let err_str = format!("{:?}", e);
            if let Ok(err) = e.downcast::<JsError>() {
                tracing::error!(
                    "[scene thread {scene_id:?}] script error onUpdate: {} msg {:?} @ {:?}",
                    err_str,
                    err.message,
                    err
                );
            } else {
                tracing::error!(
                    "[scene thread {scene_id:?}] script error onUpdate: {}",
                    err_str
                );
            }

            break;
        }

        let value = state.borrow().borrow::<SceneDying>().0;
        if value {
            tracing::info!("breaking from the thread {:?}", scene_id);
            break;
        }
    }

    let mut op_state = state.borrow_mut();
    let logs = op_state.take::<SceneLogs>();
    let sender = op_state.borrow_mut::<std::sync::mpsc::SyncSender<SceneResponse>>();
    let _ = sender.send(SceneResponse::RemoveGodotScene(scene_id, logs.0));
    runtime.v8_isolate().terminate_execution();

    tracing::info!("exiting from the thread {:?}", scene_id);

    // std::thread::sleep(Duration::from_millis(5000));
}

// helper to setup, acquire, run and return results from a script function
async fn run_script(
    runtime: &mut deno_core::JsRuntime,
    script: &v8::Global<v8::Value>,
    fn_name: &str,
    arg_fn: impl for<'a> Fn(&mut v8::HandleScope<'a>) -> Vec<v8::Local<'a, v8::Value>>,
) -> Result<(), AnyError> {
    // set up scene i/o
    let op_state = runtime.op_state();
    op_state.borrow_mut().put(());

    let promise = {
        let scope = &mut runtime.handle_scope();
        let script_this = v8::Local::new(scope, script.clone());
        // get module
        let script = v8::Local::<v8::Object>::try_from(script_this).unwrap();

        // get function
        let target_function =
            v8::String::new_from_utf8(scope, fn_name.as_bytes(), v8::NewStringType::Internalized)
                .unwrap();
        let Some(target_function) = script.get(scope, target_function.into()) else {
            // function not define, is that an error ?
            // debug!("{fn_name} is not defined");
            return Err(AnyError::msg(format!("{fn_name} is not defined")));
        };
        let Ok(target_function) = v8::Local::<v8::Function>::try_from(target_function) else {
            // error!("{fn_name} is not a function");
            return Err(AnyError::msg(format!("{fn_name} is not a function")));
        };

        // get args
        let args = arg_fn(scope);

        // call
        let res = target_function.call(scope, script_this, &args);
        let Some(res) = res else {
            // error!("{fn_name} did not return a promise");
            return Err(AnyError::msg(format!("{fn_name} did not return a promise")));
        };

        drop(args);
        v8::Global::new(scope, res)
    };

    let f = runtime.resolve_value(promise);
    f.await.map(|_| ())
}

// synchronously returns a string containing JS code from the file system
#[op(v8)]
fn op_require(
    state: &mut OpState,
    module_spec: String,
) -> Result<String, deno_core::error::AnyError> {
    match module_spec.as_str() {
        // user module load
        "~scene.js" => Ok(state.take::<SceneJsFileContent>().0),
        "~utils.js" => Ok(include_str!("js_modules/utils.js").to_owned()),
        // core module load
        "~system/CommunicationsController" => {
            Ok(include_str!("js_modules/CommunicationsController.js").to_owned())
        }
        "~system/EngineApi" => Ok(include_str!("js_modules/EngineApi.js").to_owned()),
        "~system/EnvironmentApi" => Ok(include_str!("js_modules/EnvironmentApi.js").to_owned()),
        "~system/EthereumController" => {
            Ok(include_str!("js_modules/EthereumController.js").to_owned())
        }
        "~system/Players" => Ok(include_str!("js_modules/Players.js").to_owned()),
        "~system/PortableExperiences" => {
            Ok(include_str!("js_modules/PortableExperiences.js").to_owned())
        }
        "~system/RestrictedActions" => {
            Ok(include_str!("js_modules/RestrictedActions.js").to_owned())
        }
        "fetch" => Ok(include_str!("js_modules/fetch.js").to_owned()),
        "ws" => Ok(include_str!("js_modules/ws.js").to_owned()),
        "~system/Runtime" => Ok(include_str!("js_modules/Runtime.js").to_owned()),
        "~system/Scene" => Ok(include_str!("js_modules/Scene.js").to_owned()),
        "~system/SignedFetch" => Ok(include_str!("js_modules/SignedFetch.js").to_owned()),
        "~system/Testing" => Ok(include_str!("js_modules/Testing.js").to_owned()),
        "~system/UserActionModule" => Ok(include_str!("js_modules/UserActionModule.js").to_owned()),
        "~system/UserIdentity" => Ok(include_str!("js_modules/UserIdentity.js").to_owned()),
        "~system/CommsApi" => Ok(include_str!("js_modules/CommsApi.js").to_owned()),
        "env" => Ok(get_env_for_scene(state)),
        _ => Err(generic_error(format!(
            "invalid module request `{module_spec}`"
        ))),
    }
}

#[op(v8)]
fn op_log(state: Rc<RefCell<OpState>>, message: String, immediate: bool) {
    if !is_scene_log_enabled() {
        return;
    }

    if immediate {
        tracing::info!("{}", message);
    }
    tracing::debug!("{}", message);

    let time = state.borrow().borrow::<SceneElapsedTime>().0;
    state
        .borrow_mut()
        .borrow_mut::<SceneLogs>()
        .0
        .push(SceneLogMessage {
            timestamp: time as f64,
            level: SceneLogLevel::Log,
            message,
        })
}

#[op(v8)]
fn op_error(state: Rc<RefCell<OpState>>, message: String, immediate: bool) {
    if !is_scene_log_enabled() {
        return;
    }

    if immediate {
        tracing::error!("{}", message);
    }
    tracing::debug!("{}", message);

    let time = state.borrow().borrow::<SceneElapsedTime>().0;
    state
        .borrow_mut()
        .borrow_mut::<SceneLogs>()
        .0
        .push(SceneLogMessage {
            timestamp: time as f64,
            level: SceneLogLevel::SceneError,
            message,
        })
}

#[derive(Serialize)]
pub struct SceneEnv {
    pub enable_know_env: bool,
    pub testing_enable: bool,
}

fn get_env_for_scene(state: &mut OpState) -> String {
    let scene_env = state.borrow::<SceneEnv>();
    if scene_env.enable_know_env {
        let scene_env_json = serde_json::to_string(scene_env).unwrap();
        format!("module.exports = {}", scene_env_json)
    } else {
        "module.exports = {}".to_owned()
    }
}
