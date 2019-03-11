//! # `webdav-server` is a webdav server that handles user-accounts.
//!
//! This is a webdav server that allows access to a users home directory,
//! just like an ancient FTP server would (remember those?).
//!
//! Right now, this server does not implement TLS or logging. The general idea
//! is that most people put a reverse-proxy in front of services like this
//! anyway, like NGINX, that can do TLS and logging.
//!
#![feature(async_await, await_macro, futures_api)]

#[macro_use] extern crate clap;
#[macro_use] extern crate log;
#[macro_use] extern crate lazy_static;
#[macro_use] extern crate serde_derive;

mod cache;
mod cached;
mod config;
mod either;
mod rootfs;
mod suid;
mod unixuser;
mod userfs;

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use net2;
use std::process::exit;
use std::sync::Arc;

use futures::prelude::*;
use futures::{self, future, future::Either};
use bytes::Bytes;
use hyper;
use http;
use http::status::StatusCode;
use env_logger;
use tokio;

use tokio_pam::PamAuth;
use webdav_handler::typed_headers::{HeaderMapExt, Authorization, Basic};
use webdav_handler::{DavConfig, DavHandler, webpath::WebPath};
use webdav_handler::{ls::DavLockSystem, localfs::LocalFs, fakels::FakeLs};

use crate::userfs::UserFs;
use crate::rootfs::RootFs;
use crate::suid::switch_ugid;
use crate::either::*;

static PROGNAME: &'static str = "webdav-server";

// Contains "state" and a handle to the config.
#[derive(Clone)]
struct Server {
    dh:             DavHandler,
    pam_auth:       PamAuth,
    users_path:     Arc<String>,
    config:         Arc<config::Config>,
}

type BoxedResponse = Box<Future<Item=hyper::Response<hyper::Body>, Error=std::io::Error> + Send>;

// Server implementation.
impl Server {

    // Constructor.
    pub fn new(config: Arc<config::Config>, auth: PamAuth) -> Self {
        let mut methods = webdav_handler::AllowedMethods::none();
        if let Some(ref rootfs) = config.rootfs {
            methods.add(webdav_handler::Method::Head);
            methods.add(webdav_handler::Method::Get);
            if rootfs.webdav.unwrap_or(true) {
                methods.add(webdav_handler::Method::PropFind);
                methods.add(webdav_handler::Method::Options);
            }
        }
        let dh = DavHandler::new_with(DavConfig{
            allow:  Some(methods),
            ..DavConfig::default()
        });

        // base path of the users.
        let users_path = match config.users {
            Some(ref users) => users.path.replace(":username", ""),
            None => "-".to_string(),
        };

        Server{
            dh:             dh,
            pam_auth:       auth,
            config:         config,
            users_path:     Arc::new(users_path),
        }
    }

    // get locksystem. FIXME: check user-agent header.
    fn locksystem(&self) -> Option<Box<DavLockSystem>> {
        match self.config.webdav.locksystem.as_str() {
            ""|"fakels" => Some(FakeLs::new()),
            _ => None,
        }
    }

    // get the user path from config.users.path.
    fn user_path(&self, user: &str) -> String {
        match self.config.users {
            Some(ref users) => {
                // replace :user with the username.
                users.path.replace(":username", user)
            },
            None => {
                // something that can never match.
                "-".to_string()
            },
        }
    }

    // check the initial path. it must match either the rootfs,
    // or the path to the users part of the hierarchy.
    fn check_path(&self, uri: &http::uri::Uri) -> Result<(String, String, bool), StatusCode> {

        // first normalize the path.
        let path = match WebPath::from_uri(uri, "") {
            Ok(path) => path.as_utf8_string_with_prefix(),
            Err(_) => return Err(StatusCode::BAD_REQUEST),
        };

        // get first segment of the path.
        let x = path.splitn(3, "/").collect::<Vec<&str>>();
        if x.len() < 2 {
            // can't happen, means there was no "/" in the path.
            return Err(StatusCode::BAD_REQUEST);
        }
        let is_root = x.len() < 3;
        let first_seg = x[1].to_string();

        // Either it's the root filesystem, or the prefix must match.
        debug!("XXX path: {}, users_path: {}", path, self.users_path);
        if (is_root && self.config.rootfs.is_some()) || path.starts_with(self.users_path.as_str()) {
            Ok((path, first_seg, is_root))
        } else {
            Err(StatusCode::NOT_FOUND)
        }
    }

    // handle a request.
    fn handle(&self, req: hyper::Request<hyper::Body>) -> BoxedResponse {

        // interpret the path.
        let (path, first_seg, is_root) = match self.check_path(req.uri()) {
            Ok(x) => x,
            Err(status) => return Box::new(self.error(status)),
        };

        // If we ask for "/" or "/file" with GET or HEAD, serve from local fs.
        let mut is_realroot = false;
        let method = req.method();
        if is_root && (method == &http::Method::GET || method == &http::Method::HEAD) {
            // if rootfs.auth is set, wait until after authentication.
            if let Some(ref rootfs) = self.config.rootfs {
                if rootfs.auth == false {
                    return Box::new(self.handle_realroot(req, first_seg));
                }
            }
            is_realroot = true;
        }

        // we must have a login/pass
        let (user, pass) = match req.headers().typed_get::<Authorization<Basic>>() {
            Some(Authorization(Basic{
                                username,
                                password: Some(password)
                            }
            )) => (username, password),
            _ => return Box::new(self.error(StatusCode::UNAUTHORIZED)),
        };

        let pam_auth = self.pam_auth.clone();
        let self2 = self.clone();
        let self3 = self.clone();

        // start by checking if user exists.
        let fut = cached::CachedUser::by_name(&user)
            .map_err(|_| StatusCode::UNAUTHORIZED)
            .and_then(move |pwd| {
                // authenticate user.
                let service = self3.config.pam.service.as_str();
                cached::CachedPamAuth::auth(pam_auth, service, &pwd.name, &pass, None)
                    .map_err(|_| StatusCode::UNAUTHORIZED)
                    .map(move |_| pwd)
            })
            .then(move |res| {

                // handle errors.
                let pwd = match res {
                    Err(e) => return Either4::A(self2.error(e)),
                    Ok(res) => res,
                };

                // check minimum uid
                if let Some(min_uid) = self2.config.unix.min_uid {
                    if pwd.uid < min_uid {
                        debug!("Server::handle: {}: uid {} too low (<{})", pwd.name, pwd.uid, min_uid);
                        return Either4::A(self2.error(StatusCode::UNAUTHORIZED));
                    }
                }

                // rootfs GET/HEAD delayed until after auth.
                if is_realroot {
                    return Either4::B(self2.handle_realroot(req, first_seg));
                }

                // could be virtual root for PROPFIND/OPTIONS.
                if is_root {
                    return Either4::C(self2.handle_virtualroot(req, pwd));
                }

                // Check if username matches basedir.
                let prefix = self2.user_path(&user);
                if !path.starts_with(&prefix) {
                    // in /<something>/ but doesn't match /:user/
                    debug!("Server::handle: user {} prefix {} path {} -> 401", user, prefix, path);
                    return Either4::A(self2.error(StatusCode::UNAUTHORIZED));
                }

                // All set.
                Either4::D(self2.handle_user(req, prefix, pwd))
            });
        Box::new(fut)

    }

    fn error(&self, code: StatusCode)
        -> impl Future<Item=hyper::Response<hyper::Body>, Error=std::io::Error>
    {
        let msg = format!("<error>{} {}</error>\n",
                          code.as_u16(), code.canonical_reason().unwrap_or(""));
        let mut response = hyper::Response::builder();
        response.status(code);
        response.header("Content-Type", "text/xml");
        if code == StatusCode::UNAUTHORIZED {
            let realm = self.config.accounts.realm.as_ref().map(|s| s.as_str()).unwrap_or("Webdav Server");
            response.header("WWW-Authenticate", format!("Basic realm=\"{}\"", realm).as_str());
        }
        let resp = response.body(msg.into()).unwrap();
        futures::future::ok(resp)
    }

    fn redirect(&self, path: String)
        -> impl Future<Item=hyper::Response<hyper::Body>, Error=std::io::Error>
    {
        let resp = hyper::Response::builder()
            .status(302)
            .header("content-type", "text/plain")
            .header("location", path)
            .body("302 Moved\n".into()).unwrap();
        futures::future::ok(resp)
    }

    // serve from the local filesystem.
    fn handle_realroot(&self, req: hyper::Request<hyper::Body>, first_seg: String)
        -> impl Future<Item=hyper::Response<hyper::Body>, Error=std::io::Error>
    {
        let self2 = self.clone();
        let mut req = req;

        // If this part of the path is a valid user, redirect.
        // Otherwise serve from the local filesystem.
        cached::CachedUser::by_name(&first_seg)
            .then(move |res| {
                if res.is_ok() {
                    // first path segment is a valid username.
                    debug!("Server::handle_realroot: redirect to /{}/", first_seg);
                    return Either3::A(self2.redirect("/".to_string() + &first_seg + "/"));
                }

                match self2.config.rootfs {
                    Some(ref rootfs) => {
                        debug!("Server::handle_realroot: serving {:?}", req.uri());
                        if first_seg == "" {
                            let index = rootfs.index.as_ref().map(|s| s.as_str()).unwrap_or("index.html");
                            let index = "/".to_string() + index;
                            if let Ok(pq) = http::uri::PathAndQuery::from_shared(index.into()) {
                                let mut parts = req.uri().clone().into_parts();
                                parts.path_and_query = Some(pq);
                                *req.uri_mut() = http::uri::Uri::from_parts(parts).unwrap();
                            }
                        }
                        let fs = LocalFs::new(&rootfs.directory, true);
                        let config = DavConfig {
                            fs:         Some(fs),
                            ..DavConfig::default()
                        };
                        Either3::B(self2.run_davhandler(req, config))
                    },
                    None => Either3::C(self2.error(StatusCode::NOT_FOUND))
                }
            })
    }

    // virtual root filesytem for PROPFIND/OPTIONS in "/".
    fn handle_virtualroot(&self, req: hyper::Request<hyper::Body>, pwd: Arc<unixuser::User>)
        -> impl Future<Item=hyper::Response<hyper::Body>, Error=std::io::Error>
    {
        match self.config.rootfs {
            Some(ref _rootfs) => {
                debug!("Server::handle_virtualroot: /");
                let ugid = match self.config.accounts.setuid {
                    true => Some((pwd.uid, pwd.gid)),
                    false => None,
                };
                // Only pass in the user if the base of the users tree
                // is the same as the base root directory (right now always "/").
                let user = if self.users_path.as_str() == "/" {
                    pwd.name.clone()
                } else {
                    "".to_string()
                };
                let fs = RootFs::new(&pwd.dir, user, ugid);
                let config = DavConfig {
                    fs:         Some(fs),
                    ls:         self.locksystem(),
                    principal:  Some(pwd.name.to_string()),
                    ..DavConfig::default()
                };
                Either::A(self.run_davhandler(req, config))
            },
            None => Either::B(self.error(StatusCode::NOT_FOUND))
        }
    }

    fn handle_user(&self, req: hyper::Request<hyper::Body>, prefix: String, pwd: Arc<unixuser::User>)
        -> impl Future<Item=hyper::Response<hyper::Body>, Error=std::io::Error>
    {
        match self.config.users {
            Some(ref _users) => {
                let ugid = match self.config.accounts.setuid {
                    true => Some((pwd.uid, pwd.gid)),
                    false => None,
                };
                let fs = UserFs::new(&pwd.dir, ugid, true);
                debug!("Server::handle_user: in userdir {} prefix {} ", pwd.name, prefix);
                let config = DavConfig {
                    prefix:     Some(prefix),
                    fs:         Some(fs),
                    ls:         self.locksystem(),
                    principal:  Some(pwd.name.to_string()),
                    ..DavConfig::default()
                };
                Either::A(self.run_davhandler(req, config))
            },
            None => Either::B(self.error(StatusCode::NOT_FOUND))
        }
    }

    fn run_davhandler(&self, req: hyper::Request<hyper::Body>, config: DavConfig)
        -> impl Future<Item=hyper::Response<hyper::Body>, Error=std::io::Error>
    {
        // transform hyper::Request into http::Request, run handler,
        // then transform http::Response into hyper::Response.
        let (parts, body) = req.into_parts();
        let body = body.map(|item| Bytes::from(item));
        let req = http::Request::from_parts(parts, body);
        self.dh.handle_with(config, req)
            .and_then(|resp| {
                let (parts, body) = resp.into_parts();
                let body = hyper::Body::wrap_stream(body);
                Ok(hyper::Response::from_parts(parts, body))
            })
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // command line option processing.
    let matches = clap_app!(webdav_server =>
        (version: "0.1")
        (@arg CFG: -c --config +takes_value "configuration file (/etc/webdav-server.toml)")
        (@arg PORT: -p --port +takes_value "listen to this port on localhost only")
        (@arg DIR: -d --dir +takes_value "override local directory to serve")
    ).get_matches();

    let dir = matches.value_of("DIR");
    let port = matches.value_of("PORT");
    let cfg = matches.value_of("CFG").unwrap_or("/etc/webdav-server.toml");

    // read config.
    let mut config = match config::read(cfg.clone()) {
        Err(e) => {
            eprintln!("{}: {}: {}", PROGNAME, cfg, e);
            exit(1);
        },
        Ok(c) => c,
    };
    config::check(cfg.clone(), &config);

    // override parts of the config with command line options.
    if let Some(dir) = dir {
        if config.rootfs.is_none() {
            eprintln!("{}: [rootfs] section missing", cfg);
            exit(1);
        }
        config.rootfs.as_mut().unwrap().directory = dir.to_owned();
    }
    if let Some(port) = port {
        let localhosts = vec![
            ("127.0.0.1:".to_string() + port).parse::<SocketAddr>().unwrap(),
            ("[::]:".to_string() + port).parse::<SocketAddr>().unwrap(),
        ];
        config.server.listen = config::OneOrManyAddr::Many(localhosts);
    }
    let config = Arc::new(config);

    // set cache timeouts.
    if let Some(timeout) = config.pam.cache_timeout {
        cached::set_pamcache_timeout(timeout);
    }
    if let Some(timeout) = config.unix.cache_timeout {
        cached::set_pwcache_timeout(timeout);
    }

    // resolve addresses.
    let addrs = match config.server.listen.clone().to_socket_addrs() {
        Err(e) => {
            eprintln!("{}: [server] listen: {:?}", cfg, e);
            exit(1);
        },
        Ok(a) => a,
    };

    // get pam task and handle, get a runtime, and start the pam task.
    let (pam, pam_task) = PamAuth::lazy_new(config.pam.threads.clone())?;
    let mut rt = tokio::runtime::Runtime::new()?;
    rt.spawn(pam_task.map_err(|_e| debug!("pam_task returned error {}", _e)));

    // start servers (one for each listen address).
    let dav_server = Server::new(config.clone(), pam);
    let mut servers = Vec::new();
    for sockaddr in addrs {
        let listener = match make_listener(&sockaddr) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("{}: listener on {:?}: {}", PROGNAME, &sockaddr, e);
                exit(1);
            },
        };
        let dav_server = dav_server.clone();
        let make_service = move || {
            let dav_server = dav_server.clone();
            hyper::service::service_fn(move |req| {
                dav_server.handle(req)
            })
        };
        println!("Listening on http://{:?}", sockaddr);
        let server = hyper::Server::from_tcp(listener)?
            .serve(make_service)
            .map_err(|e| eprintln!("server error: {}", e));
        servers.push(server);
    }

    // drop privs.
    match (&config.server.uid, &config.server.gid) {
        (&Some(uid), &Some(gid)) => switch_ugid(uid, gid),
        _ => {},
    }

    // run all servers and wait for them to finish.
    let servers = future::join_all(servers).then(|_| Ok::<_, hyper::Error>(()));
    let _ = rt.block_on_all(servers);

    Ok(())
}

// Make a new TcpListener, and if it's a V6 listener, set the
// V6_V6ONLY socket option on it.
fn make_listener(addr: &SocketAddr) -> io::Result<std::net::TcpListener> {
    let s = if addr.is_ipv6() {
        let s = net2::TcpBuilder::new_v6()?;
        s.only_v6(true)?;
        s
    } else {
        net2::TcpBuilder::new_v4()?
    };
    s.reuse_address(true)?;
    s.bind(addr)?;
    s.listen(128)
}

