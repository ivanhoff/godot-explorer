use reqwest::Client;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::sync::{oneshot, Semaphore};

use crate::tools::network_inspector::{
    NetworkInspectEvent, NetworkInspectRequestPayload, NetworkInspectResponsePayload,
    NetworkInspectorId, NetworkInspectorSender, NETWORK_INSPECTOR_ENABLE,
};

use super::request_response::{
    RequestOption, RequestResponse, RequestResponseError, ResponseEnum, ResponseType,
};

#[derive(Debug)]
struct QueueRequest {
    id: u32,
    priority: usize,
    request_option: Option<RequestOption>,
    response_sender: oneshot::Sender<Result<RequestResponse, RequestResponseError>>,

    network_inspector_id: NetworkInspectorId,
    network_inspector_sender: Option<NetworkInspectorSender>,
}

impl PartialEq for QueueRequest {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for QueueRequest {
    fn assert_receiver_is_total_eq(&self) {}
}

impl Ord for QueueRequest {
    fn cmp(&self, other: &Self) -> Ordering {
        other.priority.cmp(&self.priority)
    }
}

impl PartialOrd for QueueRequest {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug)]
pub struct HttpQueueRequester {
    client: Arc<Client>,
    queue: Arc<Mutex<BinaryHeap<QueueRequest>>>,
    semaphore: Arc<Semaphore>,
    inspector_sender: Option<NetworkInspectorSender>,
}

impl HttpQueueRequester {
    pub fn new(
        max_parallel_requests: usize,
        inspector_sender: Option<NetworkInspectorSender>,
    ) -> Self {
        Self {
            client: Arc::new(Client::new()),
            queue: Arc::new(Mutex::new(BinaryHeap::new())),
            semaphore: Arc::new(Semaphore::new(max_parallel_requests)),
            inspector_sender,
        }
    }

    pub async fn request(
        &self,
        request_option: RequestOption,
        priority: usize,
    ) -> Result<RequestResponse, RequestResponseError> {
        let (response_sender, response_receiver) = oneshot::channel();

        let (network_inspector_id, network_inspector_sender) = if NETWORK_INSPECTOR_ENABLE
            .load(std::sync::atomic::Ordering::Relaxed)
            && self.inspector_sender.is_some()
        {
            let (req_id, event) = NetworkInspectEvent::new_request(NetworkInspectRequestPayload {
                requester: "global".into(),
                url: request_option.url.clone(),
                method: request_option.method.clone(),
                body: request_option.body.clone(),
                headers: request_option.headers.clone(),
            });

            let inspector_sender = self
                .inspector_sender
                .clone()
                .expect("already checked for some");
            let _ = inspector_sender.send(event).await;
            (req_id, Some(inspector_sender))
        } else {
            (NetworkInspectorId::INVALID, None)
        };

        let http_request = QueueRequest {
            id: request_option.id,
            priority,
            request_option: Some(request_option),
            response_sender,

            network_inspector_id,
            network_inspector_sender,
        };

        self.queue.lock().unwrap().push(http_request);
        self.process_queue().await;
        response_receiver.await.unwrap()
    }

    async fn process_queue(&self) {
        let queue = Arc::clone(&self.queue);
        let semaphore = Arc::clone(&self.semaphore);
        let client = self.client.clone();

        tokio::spawn(async move {
            let _permit = semaphore.acquire_owned().await;
            let request = {
                let mut queue = queue.lock().unwrap();
                queue.pop()
            };

            if let Some(mut queue_request) = request {
                let request_option = queue_request.request_option.take().unwrap();
                let mut response_result = Self::process_request(
                    client,
                    request_option,
                    queue_request.network_inspector_id,
                    queue_request.network_inspector_sender.clone(),
                )
                .await;

                if queue_request.network_inspector_id.is_valid() {
                    if let Some(network_inspector_sender) =
                        queue_request.network_inspector_sender.as_ref()
                    {
                        let network_inspect_response: Result<
                            (NetworkInspectResponsePayload, Option<String>),
                            String,
                        > = match &mut response_result {
                            Ok(response) => {
                                let response_data = match &response.response_data {
                                    Ok(ResponseEnum::String(data)) => {
                                        Some(data.chars().take(10240).collect())
                                    }
                                    Ok(ResponseEnum::Json(Ok(data))) => {
                                        Some(data.to_string().chars().take(10240).collect())
                                    }
                                    Ok(ResponseEnum::ToFile(Ok(path))) => Some(path.clone()),
                                    _ => None,
                                };

                                Ok((
                                    NetworkInspectResponsePayload {
                                        status_code: response.status_code,
                                        headers: response.headers.take(),
                                    },
                                    response_data,
                                ))
                            }
                            Err(err) => Err(err.error_message.clone()),
                        };

                        let inspect_event = NetworkInspectEvent::new_full_response(
                            queue_request.network_inspector_id,
                            network_inspect_response,
                        );
                        if let Err(err) = network_inspector_sender.try_send(inspect_event) {
                            tracing::error!("Error sending inspect event: {}", err);
                        }
                    }
                }

                let _ = queue_request.response_sender.send(response_result);
            }
        });
    }

    async fn process_request(
        client: Arc<Client>,
        mut request_option: RequestOption,
        // TODO: for a granular inspection, we need to pass the sender here
        network_inspector_id: NetworkInspectorId,
        _maybe_network_inspector_sender: Option<NetworkInspectorSender>,
    ) -> Result<RequestResponse, RequestResponseError> {
        let timeout = request_option
            .timeout
            .unwrap_or(std::time::Duration::from_secs(60));
        let mut request = client
            .request(request_option.method.clone(), request_option.url.clone())
            .timeout(timeout);

        if let Some(body) = request_option.body.take() {
            request = request.body(body);
        }

        if let Some(headers) = request_option.headers.take() {
            for (key, value) in headers {
                request = request.header(key, value);
            }
        }

        let map_err_func = |e: reqwest::Error| RequestResponseError {
            id: request_option.id,
            error_message: e.to_string(),
        };

        let response = request.send().await.map_err(map_err_func)?;
        let status_code = response.status();

        let headers = if network_inspector_id.is_valid() {
            let headers = response
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();
            Some(headers)
        } else {
            None
        };

        let response_data = match request_option.response_type.clone() {
            ResponseType::AsString => {
                ResponseEnum::String(response.text().await.map_err(map_err_func)?)
            }
            ResponseType::AsBytes => {
                ResponseEnum::Bytes(response.bytes().await.map_err(map_err_func)?.to_vec())
            }
            ResponseType::ToFile(file_path) => {
                let content = response.bytes().await.map_err(map_err_func)?.to_vec();
                let mut file = tokio::fs::File::create(file_path.clone())
                    .await
                    .map_err(|e| RequestResponseError {
                        id: request_option.id,
                        error_message: e.to_string(),
                    })?;
                let result = file.write_all(&content).await;
                let result = result.map(|_| file_path);
                ResponseEnum::ToFile(result)
            }
            ResponseType::AsJson => {
                let json_string = &response.text().await.map_err(map_err_func)?;
                ResponseEnum::Json(serde_json::from_str(json_string))
            }
        };

        Ok(RequestResponse {
            headers,
            request_option,
            status_code,
            response_data: Ok(response_data),
        })
    }
}
