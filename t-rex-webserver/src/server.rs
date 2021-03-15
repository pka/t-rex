//
// Copyright (c) Pirmin Kalberer. All rights reserved.
// Licensed under the MIT License. See LICENSE file in the project root for full license information.
//

use crate::core::config::ApplicationCfg;
use crate::mvt_service::MvtService;
use crate::runtime_config::{config_from_args, service_from_args};
use crate::static_files::StaticFiles;
use actix_cors::Cors;
use actix_files as fs;
use actix_web::dev::BodyEncoding;
use actix_web::http::{header, ContentEncoding};
use actix_web::middleware::Compress;
use actix_web::{guard, middleware, web, App, HttpRequest, HttpResponse, HttpServer, Result};
use clap::ArgMatches;
use log::Level;
use num_cpus;
use open;
use std::collections::HashMap;
use std::str;
use std::str::FromStr;

static DINO: &'static str = "             xxxxxxxxx
        xxxxxxxxxxxxxxxxxxxxxxxx
      xxxxxxxxxxxxxxxxxxxxxxxxxxxx
     xxxxxxxxxxxxxxxxxxxxxxxxx xxxx
     xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
    xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
    xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
   xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
 xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx xxxxxxxxxxxxxx
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx  xxxxxxxxxxxxxx
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx   xxxxxxxxxxxxx
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx   xxxxxxxxxx
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx     xxxxxx
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx      x
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
xxxxxxxxxxxxxxxxxxxxxxxxxx    xxxxxxxxxxx
xxxxxxxxxxxxxx                   xxxxxx
xxxxxxxxxxxx
xxxxxxxxxxx
xxxxxxxxxx
xxxxxxxxx
xxxxxxx
xxxxxx
xxxxxxx";

async fn mvt_metadata(service: web::Data<MvtService>) -> Result<HttpResponse> {
    let json = service.get_mvt_metadata()?;
    Ok(HttpResponse::Ok().json(&json))
}

/// Font list for Maputnik
async fn fontstacks() -> Result<HttpResponse> {
    Ok(HttpResponse::Ok().json(&["Roboto Medium", "Roboto Regular"]))
}

// Include method fonts() which returns HashMap with embedded font files
include!(concat!(env!("OUT_DIR"), "/fonts.rs"));

/// Fonts for Maputnik
/// Example: /fonts/Open%20Sans%20Regular,Arial%20Unicode%20MS%20Regular/0-255.pbf
async fn fonts_pbf(params: web::Path<(String, String)>) -> Result<HttpResponse> {
    let fontpbfs = fonts();
    let fontlist = &params.as_ref().0;
    let range = &params.as_ref().1;
    let mut fonts = fontlist.split(",").collect::<Vec<_>>();
    fonts.push("Roboto Regular"); // Fallback
    let mut resp = HttpResponse::NotFound().finish();
    for font in fonts {
        let key = format!("fonts/{}/{}.pbf", font.replace("%20", " "), range);
        debug!("Font lookup: {}", key);
        if let Some(pbf) = fontpbfs.get(&key as &str) {
            resp = HttpResponse::Ok()
                .content_type("application/x-protobuf")
                // data is already gzip compressed
                .encoding(ContentEncoding::Identity)
                .append_header((header::CONTENT_ENCODING, "gzip"))
                .body(*pbf); // TODO: chunked response
            break;
        }
    }
    Ok(resp)
}

fn req_baseurl(req: &HttpRequest) -> String {
    let conninfo = req.connection_info();
    format!("{}://{}", conninfo.scheme(), conninfo.host())
}

async fn tileset_tilejson(
    service: web::Data<MvtService>,
    tileset: web::Path<String>,
    req: HttpRequest,
) -> Result<HttpResponse> {
    let url = req_baseurl(&req);
    let json = web::block(move || service.get_tilejson(&url, &tileset).ok()).await?;
    Ok(HttpResponse::Ok().json(&json))
}

async fn tileset_style_json(
    service: web::Data<MvtService>,
    tileset: web::Path<String>,
    req: HttpRequest,
) -> Result<HttpResponse> {
    let json = service.get_stylejson(&req_baseurl(&req), &tileset)?;
    Ok(HttpResponse::Ok().json(&json))
}

async fn tileset_metadata_json(
    service: web::Data<MvtService>,
    tileset: web::Path<String>,
) -> Result<HttpResponse> {
    let json = web::block(move || service.get_mbtiles_metadata(&tileset).ok()).await?;
    Ok(HttpResponse::Ok().json(&json))
}

async fn tile_pbf(
    config: web::Data<ApplicationCfg>,
    service: web::Data<MvtService>,
    params: web::Path<(String, u8, u32, u32)>,
    req: HttpRequest,
) -> Result<HttpResponse> {
    let params = params.into_inner();
    let tileset = params.0;
    let z = params.1;
    let x = params.2;
    let y = params.3;
    let gzip = req
        .headers()
        .get(header::ACCEPT_ENCODING)
        .and_then(|headerval| {
            headerval
                .to_str()
                .ok()
                .and_then(|headerstr| Some(headerstr.contains("gzip")))
        })
        .unwrap_or(false);
    // rust-postgres starts its own Tokio runtime
    // without blocking we get 'Cannot start a runtime from within a runtime'
    let tile = web::block(move || service.tile_cached(&tileset, x, y, z, gzip, None)).await?;
    let resp = match tile {
        Some(tile) => {
            let mut r = HttpResponse::Ok();
            r.content_type("application/x-protobuf");
            if gzip {
                // data is already gzip compressed
                r.encoding(ContentEncoding::Identity)
                    .append_header((header::CONTENT_ENCODING, "gzip"));
            }
            let cache_max_age = config.webserver.cache_control_max_age.unwrap_or(300);
            r.append_header((header::CACHE_CONTROL, format!("max-age={}", cache_max_age)));
            r.body(tile) // TODO: chunked response
        }
        None => HttpResponse::NoContent().finish(),
    };
    Ok(resp)
}

lazy_static! {
    static ref STATIC_FILES: StaticFiles = StaticFiles::init();
}

async fn static_file_handler(req: HttpRequest) -> Result<HttpResponse> {
    let key = req.path()[1..].to_string();
    let resp = if let Some(ref content) = STATIC_FILES.content(None, key) {
        HttpResponse::Ok()
            .append_header((header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")) // TOOD: use Actix middleware
            .content_type(content.1)
            .body(content.0) // TODO: chunked response
    } else {
        HttpResponse::NotFound().finish()
    };
    Ok(resp)
}

#[derive(Deserialize)]
struct DrilldownParams {
    minzoom: Option<u8>,
    maxzoom: Option<u8>,
    points: String, //x1,y1,x2,y2,..
}

async fn drilldown_handler(
    service: web::Data<MvtService>,
    params: web::Query<DrilldownParams>,
) -> Result<HttpResponse> {
    let tileset = None; // all tilesets
    let progress = false;
    let points: Vec<f64> = params
        .points
        .split(",")
        .map(|v| {
            v.parse()
                .expect("Error parsing 'point' as pair of float values")
            //FIXME: map_err(|_| error::ErrorInternalServerError("...")
        })
        .collect();
    let stats = service.drilldown(tileset, params.minzoom, params.maxzoom, points, progress);
    let json = stats.as_json()?;
    Ok(HttpResponse::Ok().json(&json))
}

#[actix_web::main]
pub async fn webserver(args: ArgMatches<'static>) -> std::io::Result<()> {
    let config = config_from_args(&args);
    let host = config
        .webserver
        .bind
        .clone()
        .unwrap_or("127.0.0.1".to_string());
    let port = config.webserver.port.unwrap_or(6767);
    let bind_addr = format!("{}:{}", host, port);
    let workers = config.webserver.threads.unwrap_or(num_cpus::get() as u8);
    let mvt_viewer = config.service.mvt.viewer;
    let openbrowser =
        bool::from_str(args.value_of("openbrowser").unwrap_or("true")).unwrap_or(false);
    let static_dirs = config.webserver.static_.clone();

    let svc_config = config.clone();
    let service = web::block(move || {
        let mut service = service_from_args(&svc_config, &args);
        service.prepare_feature_queries();
        service.init_cache();
        service
    })
    .await
    .expect("service initialization failed");

    let server = HttpServer::new(move || {
        let mut app = App::new()
            .data(config.clone())
            .data(service.clone())
            .wrap(middleware::Logger::new("%r %s %b %Dms %a"))
            .wrap(Compress::default())
            .wrap(Cors::default().send_wildcard().allowed_methods(vec!["GET"]))
            .service(
                web::resource("/index.json").route(
                    web::route()
                        .guard(guard::Any(guard::Get()).or(guard::Head()))
                        .to(mvt_metadata),
                ),
            )
            .service(
                web::resource("/fontstacks.json").route(
                    web::route()
                        .guard(guard::Any(guard::Get()).or(guard::Head()))
                        .to(fontstacks),
                ),
            )
            .service(
                web::resource("/fonts.json").route(
                    web::route()
                        .guard(guard::Any(guard::Get()).or(guard::Head()))
                        .to(fontstacks),
                ),
            )
            .service(
                web::resource("/fonts/{fonts}/{range}.pbf").route(
                    web::route()
                        .guard(guard::Any(guard::Get()).or(guard::Head()))
                        .to(fonts_pbf),
                ),
            );
        for static_dir in &static_dirs {
            let dir = &static_dir.dir;
            if std::path::Path::new(dir).is_dir() {
                info!("Serving static files from directory '{}'", dir);
                app = app.service(fs::Files::new(&static_dir.path, dir));
            } else {
                warn!("Static file directory '{}' not found", dir);
            }
        }
        app = app
            .service(
                web::resource("/{tileset}.style.json").route(
                    web::route()
                        .guard(guard::Any(guard::Get()).or(guard::Head()))
                        .to(tileset_style_json),
                ),
            )
            .service(
                web::resource("/{tileset}/metadata.json").route(
                    web::route()
                        .guard(guard::Any(guard::Get()).or(guard::Head()))
                        .to(tileset_metadata_json),
                ),
            )
            .service(
                web::resource("/{tileset}.json").route(
                    web::route()
                        .guard(guard::Any(guard::Get()).or(guard::Head()))
                        .to(tileset_tilejson),
                ),
            )
            .service(
                web::resource("/{tileset}/{z}/{x}/{y}.pbf").route(
                    web::route()
                        .guard(guard::Any(guard::Get()).or(guard::Head()))
                        .to(tile_pbf),
                ),
            );
        if mvt_viewer {
            app = app.service(
                web::resource("/drilldown").route(
                    web::route()
                        .guard(guard::Any(guard::Get()).or(guard::Head()))
                        .to(drilldown_handler),
                ),
            );
            app = app.default_service(web::to(static_file_handler));
        }
        app
    })
    .workers(workers as usize)
    .bind(&bind_addr)
    .expect("Can not start server on given IP/Port")
    .shutdown_timeout(3) // default: 30s
    .run();

    if log_enabled!(Level::Info) {
        println!("{}", DINO);
    }

    if openbrowser && mvt_viewer {
        let _res = open::that(format!("http://{}:{}", &host, port));
    }

    server.await
}
