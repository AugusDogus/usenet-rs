//! Resolves NZB files against real servers and prints each manifest's files.
//!
//! cargo run --example resolve -- servers.json a.nzb [b.nzb ...]
//!
//! `servers.json` holds `{"servers": [{host, port, username, password, ...}]}`.

use usenet::{Nzb, Pool, Server, resolve};

#[derive(serde::Deserialize)]
struct Servers {
    servers: Vec<Server>,
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let config = args.next().expect("a servers.json path");
    let Servers { mut servers } = serde_json::from_slice(&std::fs::read(&config).expect("reading servers.json"))
        .expect("servers.json holds a servers array");
    for s in &mut servers {
        s.connections = s.connections.min(8);
    }
    let pool = Pool::new(servers);
    for path in args {
        let started = std::time::Instant::now();
        let nzb = match Nzb::parse(&std::fs::read(&path).expect("reading the NZB")) {
            Ok(nzb) => nzb,
            Err(e) => {
                println!("{path}: {e}");
                continue;
            }
        };
        match resolve(&nzb, &pool).await {
            Ok(m) => {
                println!("{path}: {:?}, {} files, {:.1}s", m.packing, m.files.len(), started.elapsed().as_secs_f32());
                for f in &m.files {
                    println!("  {} ({} bytes, {} pieces)", f.path, f.size, f.pieces.len());
                }
            }
            Err(e) => println!("{path}: {e}"),
        }
    }
    for r in pool.report() {
        println!("{r:?}");
    }
}
