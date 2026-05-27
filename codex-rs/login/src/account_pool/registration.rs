use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::thread;

use chrono::Utc;
use tiny_http::Header;
use tiny_http::Request;
use tiny_http::Response;

use crate::AuthDotJson;
use crate::ServerOptions;
use crate::ShutdownHandle;
use crate::pkce::PkceCodes;
use crate::pkce::generate_pkce;
use crate::server::bind_server;
use crate::server::build_authorize_url;
use crate::server::ensure_workspace_allowed;
use crate::server::exchange_code_for_tokens;
use crate::server::generate_state;
use crate::server::obtain_api_key;
use crate::server::send_response_with_disconnect;
use crate::token_data::TokenData;
use crate::token_data::parse_chatgpt_jwt_claims;

#[derive(Debug, Clone)]
pub struct AccountRegistrationStart {
    pub auth_url: String,
    pub actual_port: u16,
}

pub struct AccountRegistrationServer {
    pub auth_url: String,
    pub actual_port: u16,
    server_handle: tokio::task::JoinHandle<io::Result<AuthDotJson>>,
    shutdown_handle: ShutdownHandle,
}

impl AccountRegistrationServer {
    pub async fn block_until_done(self) -> io::Result<AuthDotJson> {
        self.server_handle.await.map_err(|err| {
            io::Error::other(format!("account registration server panicked: {err:?}"))
        })?
    }

    pub fn cancel(&self) {
        self.shutdown_handle.shutdown();
    }

    pub fn cancel_handle(&self) -> ShutdownHandle {
        self.shutdown_handle.clone()
    }

    pub fn start(&self) -> AccountRegistrationStart {
        AccountRegistrationStart {
            auth_url: self.auth_url.clone(),
            actual_port: self.actual_port,
        }
    }
}

pub fn run_account_registration_server(
    opts: ServerOptions,
) -> io::Result<AccountRegistrationServer> {
    let pkce = generate_pkce();
    let state = opts.force_state.clone().unwrap_or_else(generate_state);
    let server = bind_server(opts.port)?;
    let actual_port = match server.server_addr().to_ip() {
        Some(addr) => addr.port(),
        None => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "Unable to determine the registration server port",
            ));
        }
    };
    let server = Arc::new(server);
    let redirect_uri = format!("http://localhost:{actual_port}/auth/callback");
    let auth_url = build_authorize_url(
        &opts.issuer,
        &opts.client_id,
        &redirect_uri,
        &pkce,
        &state,
        opts.forced_chatgpt_workspace_id.as_deref(),
    );
    if opts.open_browser {
        let _ = webbrowser::open(&auth_url);
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Request>(16);
    let _server_handle = {
        let server = Arc::clone(&server);
        thread::spawn(move || -> io::Result<()> {
            while let Ok(request) = server.recv() {
                match tx.blocking_send(request) {
                    Ok(()) => {}
                    Err(error) => {
                        return Err(io::Error::other(format!(
                            "failed to forward registration request: {error}"
                        )));
                    }
                }
            }
            Ok(())
        })
    };

    let shutdown_notify = Arc::new(tokio::sync::Notify::new());
    let server_handle = {
        let shutdown_notify = Arc::clone(&shutdown_notify);
        tokio::spawn(async move {
            let result = loop {
                tokio::select! {
                    _ = shutdown_notify.notified() => {
                        break Err(io::Error::other("Account registration was not completed"));
                    }
                    maybe_req = rx.recv() => {
                        let Some(req) = maybe_req else {
                            break Err(io::Error::other("Account registration was not completed"));
                        };
                        let url_raw = req.url().to_string();
                        if let Some(result) = process_registration_request(
                            req,
                            &url_raw,
                            &opts,
                            &redirect_uri,
                            &state,
                            &pkce,
                        ).await? {
                            break result;
                        }
                    }
                }
            };
            server.unblock();
            result
        })
    };

    Ok(AccountRegistrationServer {
        auth_url,
        actual_port,
        server_handle,
        shutdown_handle: ShutdownHandle::new(shutdown_notify),
    })
}

async fn process_registration_request(
    req: Request,
    url_raw: &str,
    opts: &ServerOptions,
    redirect_uri: &str,
    expected_state: &str,
    pkce: &PkceCodes,
) -> io::Result<Option<io::Result<AuthDotJson>>> {
    let parsed_url = match url::Url::parse(&format!("http://localhost{url_raw}")) {
        Ok(url) => url,
        Err(err) => {
            let response =
                Response::from_string(format!("Bad Request: {err}")).with_status_code(400);
            req.respond(response)?;
            return Ok(None);
        }
    };
    match parsed_url.path() {
        "/auth/callback" => {
            let params: HashMap<String, String> = parsed_url.query_pairs().into_owned().collect();
            if params.get("state").map(String::as_str) != Some(expected_state) {
                let response = html_response(
                    400,
                    "State mismatch",
                    "This sign-in link is no longer valid. Return to Codex and try again.",
                );
                send_response_with_disconnect(req, response.0, response.1)?;
                return Ok(Some(Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "state mismatch",
                ))));
            }
            if let Some(error_code) = params.get("error") {
                let description = params
                    .get("error_description")
                    .cloned()
                    .unwrap_or_else(|| format!("OAuth callback failed: {error_code}"));
                let response = html_response(400, "Sign-in failed", &description);
                send_response_with_disconnect(req, response.0, response.1)?;
                return Ok(Some(Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    description,
                ))));
            }
            let Some(code) = params.get("code").filter(|code| !code.is_empty()) else {
                let response = html_response(
                    400,
                    "Missing authorization code",
                    "Return to Codex and retry adding the account.",
                );
                send_response_with_disconnect(req, response.0, response.1)?;
                return Ok(Some(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "missing authorization code",
                ))));
            };

            let tokens =
                exchange_code_for_tokens(&opts.issuer, &opts.client_id, redirect_uri, pkce, code)
                    .await?;
            if let Err(message) = ensure_workspace_allowed(
                opts.forced_chatgpt_workspace_id.as_deref(),
                &tokens.id_token,
            ) {
                let response = html_response(403, "Workspace restricted", &message);
                send_response_with_disconnect(req, response.0, response.1)?;
                return Ok(Some(Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    message,
                ))));
            }
            let api_key = obtain_api_key(&opts.issuer, &opts.client_id, &tokens.id_token)
                .await
                .ok();
            let auth = build_chatgpt_auth(
                api_key,
                tokens.id_token,
                tokens.access_token,
                tokens.refresh_token,
            )?;
            let response = html_response(
                200,
                "Account added",
                "You can close this tab and return to Codex.",
            );
            send_response_with_disconnect(req, response.0, response.1)?;
            Ok(Some(Ok(auth)))
        }
        "/cancel" => {
            let response = html_response(
                200,
                "Sign-in canceled",
                "You can close this tab and return to Codex.",
            );
            send_response_with_disconnect(req, response.0, response.1)?;
            Ok(Some(Err(io::Error::other(
                "Account registration was canceled",
            ))))
        }
        _ => {
            req.respond(Response::from_string("Not Found").with_status_code(404))?;
            Ok(None)
        }
    }
}

fn build_chatgpt_auth(
    api_key: Option<String>,
    id_token: String,
    access_token: String,
    refresh_token: String,
) -> io::Result<AuthDotJson> {
    let mut tokens = TokenData {
        id_token: parse_chatgpt_jwt_claims(&id_token).map_err(io::Error::other)?,
        access_token,
        refresh_token,
        account_id: None,
    };
    if let Some(account_id) = tokens.id_token.chatgpt_account_id.clone() {
        tokens.account_id = Some(account_id);
    }
    Ok(AuthDotJson {
        auth_mode: Some(codex_app_server_protocol::AuthMode::Chatgpt),
        openai_api_key: api_key,
        tokens: Some(tokens),
        last_refresh: Some(Utc::now()),
        agent_identity: None,
    })
}

fn html_response(status_code: u16, title: &str, message: &str) -> (Vec<Header>, Vec<u8>) {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head><body><h1>{title}</h1><p>{message}</p></body></html>"
    )
    .into_bytes();
    let mut headers = Vec::new();
    if let Ok(header) = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]) {
        headers.push(header);
    }
    if let Ok(header) =
        Header::from_bytes(&b"X-Codex-Status"[..], status_code.to_string().as_bytes())
    {
        headers.push(header);
    }
    (headers, body)
}
