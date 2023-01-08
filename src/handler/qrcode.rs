use std::sync::Arc;

use axum::Json;
use bytes::Bytes;
use dashmap::DashMap;
use lazy_static::lazy_static;
use rand::rngs::StdRng;
use rand::SeedableRng;
use ricq::client::{Connector, DefaultConnector};
use ricq::{
    client::NetworkStatus,
    device::Device,
    ext::reconnect::Credential,
    handler::QEvent,
    version::{get_version, Protocol},
    Client, LoginResponse, QRCodeState,
};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::bot::bots::on_login;
use crate::error::{RCError, RCResult};
use crate::handler::ConvertU8;

pub struct QRCodeClient {
    pub sig: Vec<u8>,
    pub image: Vec<u8>,
    pub state: QRCodeState,
    pub client: Arc<Client>,
    pub event_receiver: tokio::sync::broadcast::Receiver<QEvent>,
    pub network_join_handle: JoinHandle<()>,
}

lazy_static! {
    static ref CLIENTS: DashMap<Bytes, QRCodeClient> = Default::default();
}

mod base64 {
    extern crate base64;

    use serde::{de, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&base64::encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = <&str>::deserialize(deserializer)?;
        base64::decode(s).map_err(de::Error::custom)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CreateClientReq {
    pub device_seed: Option<u64>,
    pub protocol: u8,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CreateClientResp {
    #[serde(with = "base64")]
    pub sig: Vec<u8>,
    #[serde(with = "base64")]
    pub image: Vec<u8>,
}

pub async fn create(Json(req): Json<CreateClientReq>) -> RCResult<Json<CreateClientResp>> {
    let rand_seed = req.device_seed.unwrap_or_else(rand::random);
    let device = Device::random_with_rng(&mut StdRng::seed_from_u64(rand_seed));
    let protocol = match Protocol::from_u8(req.protocol) {
        Protocol::MacOS => Protocol::MacOS,
        Protocol::AndroidWatch => Protocol::AndroidWatch,
        _ => return Err(RCError::ProtocolNotSupported),
    };
    let (sender, receiver) = tokio::sync::broadcast::channel(10);
    let cli = Arc::new(Client::new(device, get_version(protocol), sender));
    let connector = DefaultConnector;
    let stream = connector.connect(&cli).await?;
    let c = cli.clone();
    let network_join_handle = tokio::spawn(async move { c.start(stream).await });
    tokio::task::yield_now().await;
    let resp = cli.fetch_qrcode().await?;

    if let QRCodeState::ImageFetch(image_fetch) = resp {
        CLIENTS.insert(
            image_fetch.sig.clone(),
            QRCodeClient {
                sig: image_fetch.sig.to_vec(),
                image: image_fetch.image_data.to_vec(),
                state: QRCodeState::ImageFetch(image_fetch.clone()),
                client: cli,
                event_receiver: receiver,
                network_join_handle,
            },
        );
        Ok(Json(CreateClientResp {
            sig: image_fetch.sig.to_vec(),
            image: image_fetch.image_data.to_vec(),
        }))
    } else {
        Err(RCError::Other("invalid qrcode_state".into()))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct QueryQRCodeReq {
    #[serde(with = "base64")]
    pub sig: Vec<u8>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct QueryQRCodeResp {
    pub state: String,
}

pub async fn query(Json(req): Json<QueryQRCodeReq>) -> RCResult<Json<QueryQRCodeResp>> {
    let sig = Bytes::from(req.sig);

    let resp = CLIENTS
        .get(&sig)
        .ok_or(RCError::ClientNotFound)?
        .client
        .query_qrcode_result(&sig)
        .await
        .map_err(RCError::RQ)?;
    let state = match resp {
        QRCodeState::ImageFetch(_) => "image_fetch",
        QRCodeState::WaitingForScan => "waiting_for_scan",
        QRCodeState::WaitingForConfirm => "waiting_for_confirm",
        QRCodeState::Timeout => "timeout",
        QRCodeState::Confirmed(_) => "confirmed",
        QRCodeState::Canceled => "canceled",
    }
    .to_string();
    CLIENTS.get_mut(&sig).ok_or(RCError::ClientNotFound)?.state = resp.clone();
    if let QRCodeState::Confirmed(confirmed) = resp {
        let (_, cli) = CLIENTS.remove(&sig).unwrap();
        let mut resp = cli
            .client
            .qrcode_login(
                &confirmed.tmp_pwd,
                &confirmed.tmp_no_pic_sig,
                &confirmed.tgt_qr,
            )
            .await
            .map_err(RCError::RQ)?;

        if let LoginResponse::DeviceLockLogin(_) = resp {
            resp = cli.client.device_lock_login().await.map_err(RCError::RQ)?;
        }
        if let LoginResponse::Success(_) = resp {
            let uin = cli.client.uin().await;
            let credential = Credential::Token(cli.client.gen_token().await);
            tracing::info!("login success: {}", uin);
            on_login(
                cli.client,
                cli.event_receiver,
                credential,
                cli.network_join_handle,
            )
            .await;
        }
    }
    Ok(Json(QueryQRCodeResp { state }))
}

#[derive(Default, Serialize)]
pub struct ListClientResp {
    pub clients: Vec<ListClientRespClient>,
}

#[derive(Default, Serialize)]
pub struct ListClientRespClient {
    #[serde(with = "base64")]
    pub sig: Vec<u8>,
    #[serde(with = "base64")]
    pub image: Vec<u8>,
    pub protocol: u8,
    pub state: String,
}

pub async fn list() -> RCResult<Json<ListClientResp>> {
    let mut clients = Vec::new();
    for c in CLIENTS.iter() {
        clients.push(ListClientRespClient {
            sig: c.sig.to_vec(),
            image: c.image.clone(),
            protocol: c.client.version().await.protocol.to_u8(),
            state: match c.state {
                QRCodeState::ImageFetch(_) => "image_fetch",
                QRCodeState::WaitingForScan => "waiting_for_scan",
                QRCodeState::WaitingForConfirm => "waiting_for_confirm",
                QRCodeState::Timeout => "timeout",
                QRCodeState::Confirmed(_) => "confirmed",
                QRCodeState::Canceled => "canceled",
            }
            .into(),
        })
    }
    Ok(Json(ListClientResp { clients }))
}

#[derive(Default, Serialize, Deserialize)]
pub struct DeleteClientReq {
    #[serde(with = "base64")]
    pub sig: Vec<u8>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct DeleteClientResp {}

pub async fn delete(Json(req): Json<DeleteClientReq>) -> RCResult<Json<DeleteClientResp>> {
    if let Some((_, cli)) = CLIENTS.remove(&Bytes::from(req.sig)) {
        cli.client.stop(NetworkStatus::Stop);
    }
    Ok(Json(DeleteClientResp {}))
}
