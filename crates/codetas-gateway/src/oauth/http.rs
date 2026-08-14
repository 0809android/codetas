use super::*;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;
pub(crate) async fn post_json(url: &str, body: Value) -> Result<Value, String> {
    let response = oauth_http_client()?
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|_| "OAuth token request failed".to_string())?;
    if !response.status().is_success() {
        return Err("OAuth token request failed".into());
    }
    response
        .json()
        .await
        .map_err(|_| "OAuth token request failed".to_string())
}

pub(crate) async fn post_form(url: &str, fields: &[(&str, &str)]) -> Result<Value, String> {
    let body = form_body(fields);
    let response = oauth_http_client()?
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .await
        .map_err(|_| "OAuth token request failed".to_string())?;
    if !response.status().is_success() {
        return Err("OAuth token request failed".into());
    }
    response
        .json()
        .await
        .map_err(|_| "OAuth token request failed".to_string())
}

pub(crate) fn oauth_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| "OAuth HTTP client could not be created".to_string())
}

pub(crate) fn pkce_pair() -> (String, String) {
    let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let digest = Sha256::digest(verifier.as_bytes());
    (verifier, URL_SAFE_NO_PAD.encode(digest))
}

pub(crate) async fn wait_for_oauth_code(
    port: u16,
    path: &str,
    expected_state: &str,
    authorize_url: &str,
) -> Result<String, String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|_| "OAuth callback could not listen on loopback".to_string())?;
    open_browser(authorize_url)?;
    let accept = async {
        loop {
            let (mut stream, _) = listener
                .accept()
                .await
                .map_err(|_| "OAuth callback was not received".to_string())?;
            let mut buffer = vec![0_u8; 8192];
            let read = stream
                .read(&mut buffer)
                .await
                .map_err(|_| "OAuth callback was not received".to_string())?;
            let request = String::from_utf8_lossy(&buffer[..read]);
            let request_line = request.lines().next().unwrap_or_default();
            let Some(target) = request_line.split_whitespace().nth(1) else {
                let _ = write_callback_response(&mut stream, false).await;
                continue;
            };
            let parsed = url::Url::parse(&format!("http://127.0.0.1{target}"))
                .map_err(|_| "OAuth callback was invalid".to_string())?;
            if parsed.path() != path {
                let _ = write_callback_response(&mut stream, false).await;
                continue;
            }
            let query: BTreeMap<String, String> = parsed
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
            if query.get("state").map(String::as_str) != Some(expected_state) {
                let _ = write_callback_response(&mut stream, false).await;
                return Err("OAuth callback state did not match".into());
            }
            let Some(code) = query.get("code").cloned().filter(|value| !value.is_empty()) else {
                let _ = write_callback_response(&mut stream, false).await;
                return Err("OAuth callback did not include an authorization code".into());
            };
            let _ = write_callback_response(&mut stream, true).await;
            return Ok(code);
        }
    };
    tokio::time::timeout(LOGIN_TIMEOUT, accept)
        .await
        .map_err(|_| "OAuth login timed out".to_string())?
}

pub(crate) async fn write_callback_response(
    stream: &mut tokio::net::TcpStream,
    success: bool,
) -> Result<(), String> {
    let body = if success {
        "<html><body>CODETAS login finished. You can close this window.</body></html>"
    } else {
        "<html><body>CODETAS login failed. Return to the app and try again.</body></html>"
    };
    let status = if success { "200 OK" } else { "400 Bad Request" };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|_| "OAuth callback response failed".to_string())
}

pub(crate) fn open_browser(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|_| "OAuth login URL is invalid".to_string())?;
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return Err("OAuth login URL is invalid".into());
    }
    let status = {
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open").arg(url).status()
        }
        #[cfg(target_os = "linux")]
        {
            std::process::Command::new("xdg-open").arg(url).status()
        }
        #[cfg(target_os = "windows")]
        {
            let safe = url.replace('"', "");
            std::process::Command::new("cmd")
                .arg("/C")
                .arg(format!("start \"\" \"{safe}\""))
                .status()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            return Err("OAuth login cannot open a browser on this platform".into());
        }
    };
    status
        .map_err(|_| "OAuth login could not open a browser".to_string())
        .and_then(|code| {
            if code.success() {
                Ok(())
            } else {
                Err("OAuth login could not open a browser".into())
            }
        })
}

