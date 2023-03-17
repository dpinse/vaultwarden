use handlebars::JsonValue;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::{
    api::{ApiResult, EmptyResult, UpdateType},
    db::models::{Cipher, Device, Folder, Send, User},
    util::get_reqwest_client,
    CONFIG,
};

use once_cell::sync::Lazy; // 1.3.1
use std::time::{Duration, Instant};

#[derive(Deserialize, Debug)]
#[allow(non_snake_case)]
struct AuthPushToken {
    access_token: String,
    expires_in: i32,
}

#[derive(Debug)]
struct LocalAuthPushToken {
    access_token: String,
    valid_until: Instant,
}

async fn get_auth_push_token() -> ApiResult<String> {
    static PUSH_TOKEN: Lazy<RwLock<LocalAuthPushToken>> = Lazy::new(|| {
        RwLock::new(LocalAuthPushToken {
            access_token: String::new(),
            valid_until: Instant::now(),
        })
    });
    let push_token = PUSH_TOKEN.read().await;

    if push_token.valid_until.saturating_duration_since(Instant::now()).as_secs() > 0 {
        debug!("Auth Push token still valid, no need for a new one");
        return Ok(push_token.access_token.clone());
    }
    drop(push_token); // Drop the read lock now

    let installation_id = CONFIG.push_installation_id();
    let client_id = format!("installation.{installation_id}");
    let client_secret = CONFIG.push_installation_key();

    let params = [
        ("grant_type", "client_credentials"),
        ("scope", "api.push"),
        ("client_id", &client_id),
        ("client_secret", &client_secret),
    ];

    let res = match get_reqwest_client().post("https://identity.bitwarden.com/connect/token").form(&params).send().await
    {
        Ok(r) => r,
        Err(e) => err!(format!("Error getting push token from bitwarden server: {e}")),
    };

    let json_pushtoken = match res.json::<AuthPushToken>().await {
        Ok(r) => r,
        Err(e) => err!(format!("Unexpected push token received from bitwarden server: {e}")),
    };

    let mut push_token = PUSH_TOKEN.write().await;
    push_token.valid_until = Instant::now()
        .checked_add(Duration::new((json_pushtoken.expires_in / 2) as u64, 0)) // Token valid for half the specified time
        .unwrap();

    push_token.access_token = json_pushtoken.access_token;

    debug!("Token still valid for {}", push_token.valid_until.saturating_duration_since(Instant::now()).as_secs());
    Ok(push_token.access_token.clone())
}

pub async fn register_push_device(user_uuid: String, device: Device) -> EmptyResult {
    if !CONFIG.push_enabled() {
        let _ = Ok::<(), ()>(());
    }
    let auth_push_token = get_auth_push_token().await?;

    //Needed to register a device for push to bitwarden :
    let data = json!({
        "userId": user_uuid,
        "deviceId": device.push_uuid,
        "identifier": device.uuid,
        "type": device.atype,
        "pushToken": device.push_token
    });

    let auth_header = format!("Bearer {}", &auth_push_token);

    get_reqwest_client()
        .post(CONFIG.push_relay_uri() + "/push/register")
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json")
        .header(AUTHORIZATION, auth_header)
        .json(&data)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

pub async fn unregister_push_device(uuid: String) -> EmptyResult {
    if !CONFIG.push_enabled() {
        let _ = Ok::<(), ()>(());
    }
    let auth_push_token = get_auth_push_token().await?;

    let auth_header = format!("Bearer {}", &auth_push_token);

    match get_reqwest_client()
        .delete(CONFIG.push_relay_uri() + "/push/" + &uuid)
        .header(AUTHORIZATION, auth_header)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => err!(format!("An error occured during device unregistration: {e}")),
    };
    Ok(())
}

pub async fn push_cipher_update(ut: i32, cipher: &Cipher, acting_device_uuid: &String, conn: &mut crate::db::DbConn) {
    // We shouldn't send a push notification on cipher update if the cipher belongs to an organization, this isn't implemented in the upstream server too.
    if cipher.organization_uuid.is_some() {
        return;
    };
    let user_uuid = match cipher.user_uuid.clone() {
        Some(c) => c,
        None => {
            debug!("Cipher has no uuid");
            return;
        }
    };

    let device = match Device::find_by_uuid_and_user(acting_device_uuid, &user_uuid, conn).await {
        Some(d) => d,
        None => {
            debug!("Device doesn't exist");
            return;
        }
    };

    let data = json!({
        "userId": user_uuid,
        "organizationId": (),
        "deviceId": device.push_uuid,
        "identifier": acting_device_uuid,
        "type": ut,
        "payload": {
            "Id": cipher.uuid,
            "UserId": cipher.user_uuid,
            "OrganizationId": (),
            "RevisionDate": cipher.updated_at
        }
    });

    send_to_push_relay(data).await;
}

pub async fn push_logout(user: &User, acting_device_uuid: Option<String>, conn: &mut crate::db::DbConn) {
    let data: JsonValue;
    if let Some(d) = acting_device_uuid {
        if let Some(device) = Device::find_by_uuid_and_user(d.as_str(), &user.uuid, conn).await {
            data = json!({
                "userId": user.uuid,
                "organizationId": (),
                "deviceId": device.push_uuid,
                "identifier": d,
                "type": UpdateType::LogOut as i32,
                "payload": {
                    "UserId": user.uuid,
                    "Date": user.updated_at
                }
            });
            send_to_push_relay(data).await;
        } else {
            debug!("Device doesn't exist");
        }
    } else {
        data = json!({
            "userId": user.uuid,
            "organizationId": (),
            "deviceId": (),
            "identifier": (),
            "type": UpdateType::LogOut as i32,
            "payload": {
                "UserId": user.uuid,
                "Date": user.updated_at
            }
        });
        send_to_push_relay(data).await;
    }
}

pub async fn push_user_update(ut: i32, user: &User) {
    let data = json!({
        "userId": user.uuid,
        "organizationId": (),
        "deviceId": (),
        "identifier": (),
        "type": ut,
        "payload": {
            "UserId": user.uuid,
            "Date": user.updated_at
        }
    });

    send_to_push_relay(data).await;
}

pub async fn push_folder_update(ut: i32, folder: &Folder, acting_device_uuid: &String, conn: &mut crate::db::DbConn) {
    let device = match Device::find_by_uuid_and_user(acting_device_uuid, &folder.user_uuid, conn).await {
        Some(d) => d,
        None => {
            debug!("Device doesn't exist");
            return;
        }
    };

    let data = json!({
        "userId": folder.user_uuid,
        "organizationId": (),
        "deviceId": device.push_uuid,
        "identifier": acting_device_uuid,
        "type": ut,
        "payload": {
            "Id": folder.uuid,
            "UserId": folder.user_uuid,
            "RevisionDate": folder.updated_at
        }
    });

    send_to_push_relay(data).await;
}

pub async fn push_send_update(ut: i32, send: &Send) {
    if send.user_uuid.is_none() {
        return;
    }

    let data = json!({
        "userId": send.user_uuid,
        "organizationId": (),
        "deviceId": (),
        "identifier": (),
        "type": ut,
        "payload": {
            "Id": send.uuid,
            "UserId": send.user_uuid,
            "RevisionDate": send.revision_date
        }
    });

    send_to_push_relay(data).await;
}

async fn send_to_push_relay(data: Value) {
    if !CONFIG.push_enabled() {
        let _ = Ok::<(), ()>(());
    }

    let auth_push_token = match get_auth_push_token().await {
        Ok(s) => s,
        Err(e) => {
            debug!("Could not get the auth push token: {}", e);
            return;
        }
    };

    let auth_header = format!("Bearer {}", &auth_push_token);

    if let Err(e) = get_reqwest_client()
        .post(CONFIG.push_relay_uri() + "/push/send")
        .header(ACCEPT, "application/json")
        .header(CONTENT_TYPE, "application/json")
        .header(AUTHORIZATION, auth_header)
        .json(&data)
        .send()
        .await
    {
        error!("An error occured while sending a send update to the push relay: {}", e);
    };
}
