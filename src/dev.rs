//! `rift dev`: a throwaway proxy for plugin authors. An in-memory permission
//! store, the messaging bus on localhost with a generated certificate, one
//! backend principal with the `backend` preset and a printed token, so a Paper
//! or Pumpkin server runs against a real Rift in one command. The dashboard and
//! HTTP API run on localhost too, so groups can be edited while testing.
use rift::config::{Admin, Config, MessagingConfig, MessagingPrincipal, Route, WebConfig};
use std::{
    collections::BTreeMap,
    env, fs, io,
    net::SocketAddr,
    path::{Path, PathBuf},
};

pub const USAGE: &str = "Usage: rift dev [--listen 127.0.0.1:25565] [--backend 127.0.0.1:25566] [--server dev]\n\
    [--bus 127.0.0.1:7443] [--admin 127.0.0.1:9091] [--web 127.0.0.1:8080|off] [--dir rift-dev]\n\
    Starts a development proxy: in-memory permission store, messaging bus on --bus with a\n\
    self-signed certificate written to --dir, and backend principal server.<server> whose token\n\
    is printed (RIFT_DEV_TOKEN, generated unless set). The dashboard and HTTP API listen on\n\
    --web and accept the admin token (RIFT_ADMIN_TOKEN, else RIFT_DEV_TOKEN). Nothing is saved.";

/// The token variable of the backend principal, also used for the admin
/// service and the dashboard unless `RIFT_ADMIN_TOKEN` is set.
const TOKEN_ENV: &str = "RIFT_DEV_TOKEN";
const ADMIN_TOKEN_ENV: &str = "RIFT_ADMIN_TOKEN";

pub struct Dev {
    pub config: Config,
    token: String,
    admin_token: String,
    server: String,
    bus: SocketAddr,
    admin: SocketAddr,
    web: Option<SocketAddr>,
    ca: PathBuf,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn secret(variable: &str) -> io::Result<Option<String>> {
    match env::var(variable) {
        Ok(value) if (32..=1024).contains(&value.len()) && !value.chars().any(char::is_control) => {
            Ok(Some(value))
        }
        Ok(_) => Err(invalid(format!(
            "{variable} must contain 32..=1024 bytes without control characters"
        ))),
        Err(_) => Ok(None),
    }
}

fn random_token() -> io::Result<String> {
    let mut bytes = [0u8; 24];
    aws_lc_rs::rand::fill(&mut bytes).map_err(|_| io::Error::other("no entropy for a token"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Parse options, write the certificate and set the token variables. Runs
/// before the async runtime starts, while the process has one thread, which
/// is what makes setting environment variables sound.
pub fn prepare(args: &[String]) -> io::Result<Dev> {
    let mut listen = "127.0.0.1:25565".to_owned();
    let mut backend = "127.0.0.1:25566".to_owned();
    let mut server = "dev".to_owned();
    let mut bus = "127.0.0.1:7443".to_owned();
    let mut admin = "127.0.0.1:9091".to_owned();
    let mut web = "127.0.0.1:8080".to_owned();
    let mut dir = PathBuf::from("rift-dev");
    let mut options = args.iter();
    while let Some(option) = options.next() {
        let value = options
            .next()
            .ok_or_else(|| invalid(format!("{option} needs a value\n{USAGE}")))?;
        match option.as_str() {
            "--listen" => listen.clone_from(value),
            "--backend" => backend.clone_from(value),
            "--server" => server.clone_from(value),
            "--bus" => bus.clone_from(value),
            "--admin" => admin.clone_from(value),
            "--web" => web.clone_from(value),
            "--dir" => dir = PathBuf::from(value),
            _ => return Err(invalid(format!("unknown option {option}\n{USAGE}"))),
        }
    }
    let bus: SocketAddr = bus
        .parse()
        .map_err(|_| invalid("--bus needs an IP address and port"))?;
    let admin: SocketAddr = admin
        .parse()
        .map_err(|_| invalid("--admin needs an IP address and port"))?;
    if !admin.ip().is_loopback() {
        return Err(invalid("--admin must be a loopback address"));
    }
    let web: Option<SocketAddr> = match web.as_str() {
        "off" => None,
        web => Some(
            web.parse()
                .map_err(|_| invalid("--web needs an IP address and port, or off"))?,
        ),
    };
    if web.is_some_and(|web| !web.ip().is_loopback()) {
        return Err(invalid("--web must be a loopback address"));
    }
    let principal = format!(
        "{}{server}",
        rift::permissions::control::BACKEND_PRINCIPAL_PREFIX
    );
    let mut config = Config::from_addresses(&listen, &backend)?;
    let target = config
        .backends
        .remove("default")
        .expect("from_addresses names its backend default");
    config.backends = BTreeMap::from([(server.clone(), target)]);
    config.routes = BTreeMap::from([("default".into(), Route::Direct(server.clone()))]);

    fs::create_dir_all(&dir)?;
    let dir = std::path::absolute(&dir)?;
    let certificate =
        rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])
            .map_err(|error| io::Error::other(format!("generating a certificate: {error}")))?;
    let ca = dir.join("ca.pem");
    let key = dir.join("key.pem");
    write_private(&ca, certificate.cert.pem().as_bytes())?;
    write_private(&key, certificate.signing_key.serialize_pem().as_bytes())?;

    let token = match secret(TOKEN_ENV)? {
        Some(token) => token,
        None => random_token()?,
    };
    let admin_token = secret(ADMIN_TOKEN_ENV)?.unwrap_or_else(|| token.clone());
    if web.is_some() && !admin_token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(invalid(format!(
            "the dashboard accepts the admin token, so {ADMIN_TOKEN_ENV} (or {TOKEN_ENV}) must be printable ASCII without spaces; or pass --web off"
        )));
    }
    // SAFETY: called from `main` before the runtime or any other thread starts.
    unsafe {
        env::set_var(TOKEN_ENV, &token);
        env::set_var(ADMIN_TOKEN_ENV, &admin_token);
    }
    let (publish, subscribe) = rift::permissions::backend_grants();
    config.messaging = Some(MessagingConfig {
        listen: Some(bus),
        certificate: Some(ca.clone()),
        private_key: Some(key),
        principals: vec![MessagingPrincipal {
            name: principal,
            token_env: TOKEN_ENV.into(),
            publish,
            subscribe,
            control: true,
        }],
        ..MessagingConfig::default()
    });
    config.admin = Some(Admin {
        listen: admin,
        token_env: ADMIN_TOKEN_ENV.into(),
        permissions: rift::config::ADMIN_PERMISSIONS
            .iter()
            .map(|permission| (*permission).to_owned())
            .collect(),
    });
    config.web = web.map(|listen| WebConfig {
        listen,
        token: Some(admin_token.clone()),
        ..WebConfig::default()
    });
    config.validate()?;
    Ok(Dev {
        config,
        token,
        admin_token,
        server,
        bus,
        admin,
        web,
        ca,
    })
}

fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub async fn run(dev: Dev) -> io::Result<()> {
    let Dev {
        config,
        token,
        admin_token,
        server,
        bus,
        admin,
        web,
        ca,
    } = dev;
    let listen = config.listeners["default"];
    let backend = config.backends[&server].address().to_owned();
    let plugin = serde_json::json!({
        "address": bus.to_string(),
        "server_name": "localhost",
        "ca": ca.display().to_string(),
        "token_env": TOKEN_ENV,
        "server": server,
    });
    println!("rift dev: development proxy; the permission store is in memory and nothing is saved");
    println!(
        "rift dev: players connect to {listen}; backend {server:?} is {backend} (offline mode)"
    );
    println!(
        "rift dev: backend plugins connect to the bus at {bus} as server.{server} (server name \"localhost\", CA {})",
        ca.display()
    );
    println!("rift dev: export {TOKEN_ENV}={token}");
    println!("rift dev: plugin configuration: {plugin}");
    println!(
        "rift dev: in rift.lua the same principal is {{ name = \"server.{server}\", token_env = \"{TOKEN_ENV}\", preset = \"backend\" }}"
    );
    println!(
        "rift dev: edit permissions with: {ADMIN_TOKEN_ENV}={admin_token} rift admin --address {admin} perms help"
    );
    if let Some(web) = web {
        println!(
            "rift dev: dashboard at http://{web}/; log in under API access with the token {admin_token}"
        );
    }
    crate::runtime::serve(config, None).await
}
