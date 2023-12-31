mod headermap;
mod method;

use std::{collections::HashMap, future::Future};

use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime, State,
};

use crate::cookie_client::{CookieClient, CookieClientPool, RedirectPolicy};

use self::{headermap::HeaderMap, method::Method};

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FetchOptions {
    response_type: PayloadType,
    #[serde(default = "default_method")]
    method: Method,
    headers: Option<HeaderMap>,
    cookies: Option<HashMap<String, String>>,
    redirect: Option<Redirect>,
    body: Option<Body>,
}

fn default_method() -> Method {
    Method::GET
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Redirect {
    None,
    Limited { max: usize },
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
enum PayloadType {
    Binary,
    Text,
    Discard,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "camelCase")]
enum Body {
    Binary(Vec<u8>),
    Text(String),
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct Response {
    url: String,
    status: u16,
    headers: HeaderMap,
    cookies: HashMap<String, String>,
    body: Option<Body>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct Error {
    url: Option<String>,
    message: String,
}

impl Error {
    fn from(e: reqwest::Error) -> Self {
        Error {
            url: e.url().map(|e| e.to_string()),
            message: format!("{}", e),
        }
    }

    fn from_std_error(e: &dyn std::error::Error) -> Self {
        Error {
            url: None,
            message: format!("{}", e),
        }
    }
}

#[tauri::command]
async fn fetch(
    state: State<'_, CookieFetchState>,
    url: String,
    options: Option<FetchOptions>,
) -> Result<Response, Error> {
    let client = state.pool.get().await;
    let url = match reqwest::Url::parse(&url) {
        Ok(v) => v,
        Err(e) => return Err(Error::from_std_error(&e)),
    };

    let Some(options) = options else {
        return proxy(
            &client,
            PayloadType::Binary,
            client.request(reqwest::Method::GET, url).send(),
        )
        .await;
    };

    let response_type = options.response_type;

    if let Some(cookies) = options.cookies {
        let mut cookies_store = client.cookie_store();
        for (name, value) in cookies {
            let cookie = reqwest_cookie_store::RawCookie::new(name, value);
            if let Some(e) = cookies_store.insert_raw(&cookie, &url).err() {
                return Err(Error::from_std_error(&e));
            }
        }
    };

    if let Some(policy) = options.redirect {
        let mut redirect_policy = client.redirect_policy();
        match policy {
            Redirect::None => *redirect_policy = RedirectPolicy::none(),
            Redirect::Limited { max } => *redirect_policy = RedirectPolicy::limited(max),
        }
    }

    let builder = client.request(options.method.into(), url);

    let builder = if let Some(headers) = options.headers {
        builder.headers(headers.into())
    } else {
        builder
    };

    let builder = if let Some(body) = options.body {
        let body = match body {
            Body::Binary(vec) => reqwest::Body::from(vec),
            Body::Text(text) => reqwest::Body::from(text),
        };
        builder.body(body)
    } else {
        builder
    };

    return proxy(&client, response_type, builder.send()).await;
}

async fn proxy(
    client: &CookieClient,
    response_type: PayloadType,
    future: impl Future<Output = Result<reqwest::Response, reqwest::Error>>,
) -> Result<Response, Error> {
    let res = match future.await {
        Ok(v) => v,
        Err(e) => return Err(Error::from(e)),
    };

    let cookies: HashMap<String, String> = client
        .cookie_store()
        .iter_any()
        .map(|e| (e.name().to_string(), e.value().to_string()))
        .collect();

    let url = res.url().to_string();
    let status = res.status().as_u16();
    let headers = res.headers().clone().into();

    let body = match response_type {
        PayloadType::Binary => match res.bytes().await {
            Ok(v) => Some(Body::Binary(v.to_vec())),
            Err(e) => return Err(Error::from(e)),
        },
        PayloadType::Text => match res.text().await {
            Ok(v) => Some(Body::Text(v)),
            Err(e) => return Err(Error::from(e)),
        },
        PayloadType::Discard => None,
    };

    let res = Response {
        url,
        status,
        headers,
        cookies,
        body,
    };

    Ok(res)
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("cookie_fetch")
        .setup(|app| {
            app.manage(CookieFetchState {
                pool: CookieClientPool::new(),
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![fetch])
        .build()
}

struct CookieFetchState {
    pool: CookieClientPool,
}
