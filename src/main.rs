mod git;
mod native;
mod render;
mod security;
mod types;

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, Response, StatusCode, header},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Semaphore;
use types::{AppError, Params, Result};

struct Config {
    root: PathBuf,
    bind: SocketAddr,
    origin: String,
    host: String,
    ssh_prefix: Option<String>,
    branch: String,
}
struct App {
    root: security::Root,
    config: Config,
    secret: Vec<u8>,
    jobs: Arc<Semaphore>,
    queue: Arc<Semaphore>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("gitt: {}", error.message);
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    // First libgit2 call in this process, before argument validation or threads.
    unsafe {
        native::initialize()?;
    }
    let config = parse_args()?;
    render::initialize()?;
    let root = security::Root::new(&config.root)?;
    security::initialize_git()?;
    let secret = security::random_hex(32)?.into_bytes();
    let listener = std::net::TcpListener::bind(config.bind)?;
    listener.set_nonblocking(true)?;
    let abi = root.confine()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .thread_keep_alive(Duration::from_secs(2))
        .build()?;
    eprintln!(
        "gitt {} listening on {} (Landlock ABI {abi})",
        env!("CARGO_PKG_VERSION"),
        config.origin
    );
    let state = Arc::new(App {
        root,
        config,
        secret,
        jobs: Arc::new(Semaphore::new(2)),
        queue: Arc::new(Semaphore::new(6)),
    });
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let router = Router::new().fallback(handle).with_state(state);
        let connections = Arc::new(Semaphore::new(16));
        loop {
            let (stream, _) = listener.accept().await?;
            let permit = match connections.clone().try_acquire_owned() {
                Ok(x) => x,
                Err(_) => {
                    drop(stream);
                    continue;
                }
            };
            let service = hyper_util::service::TowerToHyperService::new(router.clone());
            tokio::spawn(async move {
                let _permit = permit;
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder
                    .timer(hyper_util::rt::TokioTimer::new())
                    .header_read_timeout(Duration::from_secs(10))
                    .max_headers(64)
                    .max_buf_size(16384)
                    .keep_alive(false);
                let _ = tokio::time::timeout(
                    Duration::from_secs(120),
                    builder.serve_connection(hyper_util::rt::TokioIo::new(stream), service),
                )
                .await;
            });
        }
        #[allow(unreachable_code)]
        Ok::<(), AppError>(())
    })
}
fn parse_args() -> Result<Config> {
    let mut args = std::env::args_os().skip(1);
    let mut root = None;
    let mut bind: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let mut origin = None;
    let mut ssh_prefix = None;
    let mut branch = "main".to_string();
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy();
        if text == "--help" || text == "-h" {
            println!(
                "gitt — browse and create local Git repositories\n\nUsage: gitt <repository-root> [options]\n\n  --bind <IP:port>         Listen address (127.0.0.1:8080)\n  --origin <http(s)://host[:port]>  Allowed browser origin; required for wildcard binds\n  --ssh-prefix <ssh://…/>  Prefix used to display clone URLs\n  --initial-branch <name>  Branch for new repositories (main)\n  --version               Print version\n\nLinux with Landlock ABI 3+ is required. No accounts or authentication.\nAnyone who can reach the allowed origin can read and create repositories."
            );
            std::process::exit(0);
        }
        if text == "--version" {
            println!("gitt {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        if text.starts_with('-') {
            let value = args
                .next()
                .ok_or_else(|| AppError::bad(format!("Missing value for {text}.")))?
                .into_string()
                .map_err(|_| AppError::bad("Option values must be UTF-8."))?;
            match text.as_ref() {
                "--bind" => {
                    bind = value.parse().map_err(|_| {
                        AppError::bad("--bind must be a numeric IP address and port.")
                    })?
                }
                "--origin" => origin = Some(value),
                "--ssh-prefix" => ssh_prefix = Some(value),
                "--initial-branch" => branch = value,
                _ => return Err(AppError::bad(format!("Unknown option {text}."))),
            }
        } else if root.replace(PathBuf::from(arg)).is_some() {
            return Err(AppError::bad("Specify exactly one repository directory."));
        }
    }
    let root = root.ok_or_else(|| {
        AppError::bad("Usage: gitt <repository-root> [--bind IP:port]. Run --help for options.")
    })?;
    if !git2::Reference::is_valid_name(&format!("refs/heads/{branch}")) {
        return Err(AppError::bad("Invalid initial branch."));
    }
    if bind.ip().is_unspecified() && origin.is_none() {
        return Err(AppError::bad(
            "Set --origin to the browser URL when binding to all interfaces.",
        ));
    }
    if bind.port() == 0 {
        return Err(AppError::bad("Choose a nonzero listening port."));
    }
    let origin = origin
        .unwrap_or_else(|| format!("http://{bind}"))
        .trim_end_matches('/')
        .to_string();
    let uri: axum::http::Uri = origin
        .parse()
        .map_err(|_| AppError::bad("Invalid --origin URL."))?;
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri.authority().is_none()
        || uri.authority().unwrap().as_str().contains('@')
        || uri
            .path_and_query()
            .is_some_and(|p| p.as_str() != "/" && !p.as_str().is_empty())
    {
        return Err(AppError::bad(
            "--origin must be an http(s) origin without a path, credentials, or query.",
        ));
    }
    let scheme = uri.scheme_str().unwrap();
    let authority = uri.authority().unwrap();
    let host = if (scheme == "http" && authority.port_u16() == Some(80))
        || (scheme == "https" && authority.port_u16() == Some(443))
    {
        authority.host().to_ascii_lowercase()
    } else {
        authority.as_str().to_ascii_lowercase()
    };
    let origin = format!("{scheme}://{host}");
    if ssh_prefix.as_ref().is_some_and(|s| {
        !s.starts_with("ssh://") || !s.ends_with('/') || s.chars().any(char::is_control)
    }) {
        return Err(AppError::bad(
            "--ssh-prefix must start with ssh:// and end with /.",
        ));
    }
    Ok(Config {
        root,
        bind,
        origin,
        host,
        ssh_prefix,
        branch,
    })
}
async fn handle(State(app): State<Arc<App>>, request: Request<Body>) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if host != app.config.host {
        return error_response(
            AppError::new(
                403,
                "This Host is not allowed. Use the configured browser origin.",
            ),
            None,
        );
    }
    if parts.uri.scheme().is_some() || parts.uri.authority().is_some() {
        return error_response(
            AppError::bad("Use an origin-relative request target."),
            None,
        );
    }
    let path = parts.uri.path().to_string();
    let method = parts.method.clone();
    if method != axum::http::Method::GET
        && method != axum::http::Method::HEAD
        && !(method == axum::http::Method::POST && path == "/new")
    {
        return error_response(AppError::new(405, "That method is not supported."), None);
    }
    if path == "/style.css" {
        let mut response = response(200, "text/css; charset=utf-8", Body::from(render::CSS));
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            "public, max-age=3600".parse().unwrap(),
        );
        return response;
    }
    let params = if method == axum::http::Method::POST {
        if parts
            .headers
            .get(header::ORIGIN)
            .and_then(|h| h.to_str().ok())
            != Some(app.config.origin.as_str())
        {
            return error_response(
                AppError::new(
                    403,
                    "Creation requires a form submitted from the configured origin.",
                ),
                None,
            );
        }
        if parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .is_none_or(|s| {
                !s.split(';')
                    .next()
                    .unwrap_or("")
                    .eq_ignore_ascii_case("application/x-www-form-urlencoded")
            })
        {
            return error_response(
                AppError::new(415, "Submit the repository creation form."),
                None,
            );
        }
        let bytes = match tokio::time::timeout(Duration::from_secs(10), to_bytes(body, 16384)).await
        {
            Ok(Ok(b)) => b,
            Ok(Err(_)) => {
                return error_response(AppError::new(413, "Creation form is too large."), None);
            }
            Err(_) => return error_response(AppError::new(408, "Request body timed out."), None),
        };
        std::str::from_utf8(&bytes)
            .map_err(|_| AppError::bad("Invalid form encoding."))
            .and_then(Params::parse)
    } else {
        Params::parse(parts.uri.query().unwrap_or(""))
    };
    let q = match params {
        Ok(q) => q,
        Err(e) => return error_response(e, None),
    };
    if path == "/new" && method == axum::http::Method::POST {
        let cookie = parts
            .headers
            .get(header::COOKIE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .split(';')
            .map(str::trim)
            .find_map(|s| s.strip_prefix("gitt_csrf="));
        if !valid_csrf(&app, q.bytes("csrf"), cookie) {
            return error_response(
                AppError::new(
                    403,
                    "This creation form has expired or is invalid. Open New repository and try again.",
                ),
                None,
            );
        }
    }
    let queue = match app.queue.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return busy(),
    };
    let permit =
        match tokio::time::timeout(Duration::from_secs(10), app.jobs.clone().acquire_owned()).await
        {
            Ok(Ok(p)) => p,
            _ => return busy(),
        };
    let is_head = method == axum::http::Method::HEAD;
    let is_creation = method == axum::http::Method::POST;
    let job = tokio::task::spawn_blocking(move || {
        let (_queue, _permit) = (queue, permit);
        let result = route(&app, &path, &q, method == axum::http::Method::POST);
        result.unwrap_or_else(|e| {
            if path == "/files" && e.status == 422 && !q.bytes("path").is_empty() {
                let args: Vec<(&str,&[u8])> = q.0.iter().filter(|(k,_)| k.as_str() != "source").map(|(k,v)|(k.as_str(),v.as_slice())).collect();
                let mut source_args=args.clone(); source_args.push(("source",b"1"));
                let message=format!("<p>{}</p><p><a href=\"{}\">Download raw file</a> · <a href=\"{}\">View source</a></p>",render::escape(&e.message),render::escape(&render::url("/raw",&args)),render::escape(&render::url("/files",&source_args)));
                return html(422,render::layout("Preview unavailable",Some(q.bytes("repo")),&message).unwrap_or_default());
            }
            error_response(e,if matches!(path.as_str(),"/files"|"/refs"|"/history"|"/commit"|"/compare"|"/changes"|"/raw"){Some(q.bytes("repo"))}else{None})
        })
    });
    match tokio::time::timeout(Duration::from_secs(30), job).await {
        Ok(Ok(mut response)) => {
            if is_head {
                *response.body_mut() = Body::empty();
            }
            response
        }
        _ if is_creation => error_response(
            AppError::new(
                503,
                "The creation response timed out. Creation may still finish; check the repository listing before retrying. Existing destinations will not be overwritten.",
            ),
            None,
        ),
        _ => busy(),
    }
}
fn route(app: &App, path: &str, q: &Params, post: bool) -> Result<Response<Body>> {
    if path == "/new" {
        if post {
            let bare = match q.text("kind")? {
                "bare" => true,
                "checkout" => false,
                _ => return Err(AppError::bad("Choose bare or checkout.")),
            };
            let branch = if q.bytes("branch").is_empty() {
                &app.config.branch
            } else {
                q.text("branch")?
            };
            app.root.create(q.bytes("path"), bare, branch)?;
            let mut result = response(
                303,
                "text/plain; charset=utf-8",
                Body::from("Repository created."),
            );
            result.headers_mut().insert(
                header::LOCATION,
                render::url("/files", &[("repo", q.bytes("path"))])
                    .parse()
                    .unwrap(),
            );
            return Ok(result);
        }
        let token = csrf(app)?;
        let body = format!(
            "<p>Create an empty repository. Existing paths are never overwritten.</p><form method=\"post\" action=\"/new\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><p><label>Repository path <input name=\"path\" required maxlength=\"1024\" placeholder=\"team/project\" autocomplete=\"off\"></label></p><p><label>Kind <select name=\"kind\"><option value=\"bare\">Bare — push over SSH</option><option value=\"checkout\">Checkout — work directly on disk</option></select></label></p><p><label>Initial branch <input name=\"branch\" required value=\"{}\"></label></p><p>SSH transport and filesystem access are managed on the host. Pushing to a checkout’s checked-out branch follows Git’s normal refusal policy.</p><button type=\"submit\">Create repository</button></form>",
            render::escape(&token),
            render::escape(&app.config.branch)
        );
        let mut result = html(200, render::layout("New repository", None, &body)?);
        let cookie = format!(
            "gitt_csrf={token}; Path=/new; HttpOnly; SameSite=Strict; Max-Age=3600{}",
            if app.config.origin.starts_with("https:") {
                "; Secure"
            } else {
                ""
            }
        );
        result
            .headers_mut()
            .insert(header::SET_COOKIE, cookie.parse().unwrap());
        return Ok(result);
    }
    if path == "/help" {
        return Ok(html(
            200,
            render::layout("About gitt", None, include_str!("../static/help.html"))?,
        ));
    }
    if path == "/raw" {
        let data = git::raw(&app.root, q)?;
        let body = match data {
            git::Raw::Bytes(b) => Body::from(b),
            git::Raw::File(f) => Body::from_stream(tokio_util::io::ReaderStream::with_capacity(
                tokio::fs::File::from_std(f),
                16384,
            )),
        };
        let mut result = response(200, "application/octet-stream", body);
        let filename = render::display(
            q.bytes("path")
                .rsplit(|b| *b == b'/')
                .next()
                .unwrap_or(b"download"),
        );
        let encoded = percent_encoding::percent_encode(
            filename.as_bytes(),
            percent_encoding::NON_ALPHANUMERIC,
        );
        result.headers_mut().insert(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"download\"; filename*=UTF-8''{encoded}")
                .parse()
                .unwrap(),
        );
        return Ok(result);
    }
    if !matches!(
        path,
        "/" | "/files" | "/refs" | "/history" | "/commit" | "/compare" | "/changes"
    ) {
        return Err(AppError::new(404, "Page not found."));
    }
    let page = git::page(&app.root, path, q, app.config.ssh_prefix.as_deref())?;
    let repo = if path == "/" {
        None
    } else {
        Some(q.bytes("repo"))
    };
    let revision = match path {
        "/commit" => q.bytes("id"),
        "/compare" => q.bytes("to"),
        _ => q.bytes("rev"),
    };
    let document = render::layout_page(
        &page,
        repo,
        if revision.is_empty() {
            None
        } else {
            Some(revision)
        },
    )?;
    if document.len() > types::HTML_LIMIT {
        return Err(AppError::limit(
            "This view exceeds the 2 MiB rendered page limit. Narrow the path or use local Git.",
        ));
    }
    Ok(html(200, document))
}
fn csrf(app: &App) -> Result<String> {
    let payload = format!("{}.{}", now(), security::random_hex(16)?);
    let mut mac = Hmac::<Sha256>::new_from_slice(&app.secret).unwrap();
    mac.update(payload.as_bytes());
    Ok(format!(
        "{payload}.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    ))
}
fn valid_csrf(app: &App, token: &[u8], cookie: Option<&str>) -> bool {
    let Ok(token) = std::str::from_utf8(token) else {
        return false;
    };
    if cookie != Some(token) {
        return false;
    }
    let Some((payload, sig)) = token.rsplit_once('.') else {
        return false;
    };
    let Some(timestamp) = payload
        .split('.')
        .next()
        .and_then(|s| s.parse::<u64>().ok())
    else {
        return false;
    };
    let current = now();
    if timestamp > current || current - timestamp > 3600 {
        return false;
    }
    let Ok(sig) = URL_SAFE_NO_PAD.decode(sig) else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(&app.secret).unwrap();
    mac.update(payload.as_bytes());
    mac.verify_slice(&sig).is_ok()
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn html(status: u16, body: String) -> Response<Body> {
    response(status, "text/html; charset=utf-8", Body::from(body))
}
fn response(status: u16, content_type: &str, body: Body) -> Response<Body> {
    Response::builder().status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header(header::CONTENT_TYPE,content_type).header(header::CACHE_CONTROL,"no-store")
        .header("content-security-policy","default-src 'none'; style-src 'self'; img-src 'none'; script-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'")
        .header("x-content-type-options","nosniff").header("referrer-policy","no-referrer").body(body).unwrap()
}
fn error_response(e: AppError, repo: Option<&[u8]>) -> Response<Body> {
    let title = match e.status {
        404 => "Not found",
        403 => "Access denied",
        409 => "Destination exists",
        422 => "Cannot display this view",
        503 => "Busy",
        _ => "Request could not be completed",
    };
    let body = format!(
        "<p>{}</p><p><a href=\"/\">Repositories</a> · <a href=\"/new\">New repository</a></p>",
        render::escape(&e.message)
    );
    html(
        e.status,
        render::layout(title, repo, &body)
            .unwrap_or_else(|_| "Could not render this page.".to_string()),
    )
}
fn busy() -> Response<Body> {
    let mut response = error_response(
        AppError::new(
            503,
            "Both workers and the waiting queue are busy. Try again shortly.",
        ),
        None,
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, "2".parse().unwrap());
    response
}
