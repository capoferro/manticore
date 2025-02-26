// Copyright lowRISC contributors.
// Licensed under the Apache License, Version 2.0, see LICENSE for details.
// SPDX-License-Identifier: Apache-2.0

//! A virtual PA-RoT that can be spoken to over local TCP.

use std::env;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io::BufRead as _;
use std::io::BufReader;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::str;
use std::time::Duration;

use manticore::cert;
use manticore::cert::CertFormat;
use manticore::crypto::ring;
use manticore::mem::Arena;
use manticore::mem::BumpArena;
use manticore::net;
use manticore::protocol;
use manticore::protocol::capabilities;
use manticore::protocol::cerberus;
use manticore::protocol::device_id::DeviceIdentifier;
use manticore::protocol::spdm;
use manticore::server;
use manticore::server::pa_rot::PaRot;
use manticore::session::ring::Session;

use crate::support::fakes;
use crate::support::tcp;
use crate::support::tcp::TcpHostPort;

/// Options for the PA-RoT.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct Options {
    /// The protocol to speak.
    pub protocol: Protocol,

    /// A firmware version blob to report to clients.
    pub firmware_version: Vec<u8>,

    /// Vendor-specific firmware version blobs to report to clients.
    pub vendor_firmware_versions: Vec<(u8, Vec<u8>)>,

    /// A unique device identity blob to report to clients.
    pub unique_device_identity: Vec<u8>,

    /// The number of resets to report since power on.
    pub resets_since_power_on: u32,

    /// The maximum message size to report as a capability
    /// (unused by the transport).
    pub max_message_size: u16,
    /// The maximum packet size to report as a capability
    /// (unused by the transport).
    pub max_packet_size: u16,

    /// The timeout to report for a non-cryptographic operation
    /// (unused other than for capabilities requests).
    pub regular_timeout: Duration,
    /// The timeout to report for a cryptographic operation.
    /// (unused other than for capabilities requests)
    pub crypto_timeout: Duration,

    /// The device identifier to report to the client.
    pub device_id: DeviceIdentifier,

    /// The initial certificate chain to provision to the device.
    pub cert_chain: Vec<Vec<u8>>,

    /// The certificate format to parse the cert chain with.
    pub cert_format: CertFormat,

    /// The keypair to use with the certificate chain.
    pub alias_keypair: Option<KeyPairFormat>,

    /// The contents of PMR #0.
    pub pmr0: Vec<u8>,
}

/// See [`Options::protocol`].
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[allow(missing_docs)]
pub enum Protocol {
    Cerberus,
    Spdm,
}

/// See [`Options::alias_keypair`].
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub enum KeyPairFormat {
    /// An RSA PKCS#8-encoded key pair.
    RsaPkcs8(Vec<u8>),
}

impl Default for Options {
    fn default() -> Self {
        Self {
            protocol: Protocol::Cerberus,
            firmware_version: b"<version unspecified>".to_vec(),
            vendor_firmware_versions: vec![],
            unique_device_identity: b"<uid unspecified>".to_vec(),
            resets_since_power_on: 5,
            max_message_size: 1024,
            max_packet_size: 256,
            regular_timeout: Duration::from_millis(30),
            crypto_timeout: Duration::from_millis(200),
            device_id: DeviceIdentifier {
                vendor_id: 1,
                device_id: 2,
                subsys_vendor_id: 3,
                subsys_id: 4,
            },
            cert_chain: vec![],
            cert_format: CertFormat::RiotX509,
            alias_keypair: None,
            pmr0: b"<pmr0 unspecified>".to_vec(),
        }
    }
}

/// A virtual PA-RoT, implemented as a subprocess speaking TCP.
pub struct Virtual {
    child: Child,
    port: u16,
}

impl Drop for Virtual {
    fn drop(&mut self) {
        self.child.kill().unwrap();
    }
}

impl Virtual {
    /// Extracts the name of the binary-under-test from the environment.
    ///
    /// If missing, aborts the process. This will bypass the Rust test harness's
    /// attempt to continue running other tests.
    pub fn target_binary() -> &'static OsStr {
        const TARGET_BINARY: &str = "MANTICORE_E2E_TARGET_BINARY";
        lazy_static::lazy_static! {
            static ref BINARY_PATH: OsString = match env::var_os(TARGET_BINARY) {
                Some(bin) => bin,
                None => {
                    use std::io;
                    use std::io::Write;
                    use std::process;

                    let _ = writeln!(
                        io::stderr(),
                        "Could not find environment variable {}; aborting.",
                        TARGET_BINARY
                    );
                    let _ = writeln!(
                        io::stderr(),
                        "Consider using the e2e/run.sh script, instead."
                    );

                    process::exit(255);
                }
            };
        }
        &BINARY_PATH
    }

    /// Spawns a virtual PA-RoT subprocess as described by `opts`.
    pub fn spawn(opts: &Options) -> Virtual {
        log::info!("spawning server: {:#?}", opts);
        let opts = serde_json::to_string(opts).unwrap();
        let mut child = Command::new(Self::target_binary())
            .args(&["--start-pa-rot-with-options", &opts])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn server subprocess");

        // Forward stderr through the eprint! macro, so that tests can capture
        // it.
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let mut line = String::new();
        let _ = std::thread::spawn(move || loop {
            line.clear();
            match stderr.read_line(&mut line) {
                Ok(_) => eprint!("{}", line),
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => break,
                Err(e) => panic!("unexpected error from virtual rot: {}", e),
            }
        });

        // Wait until the child signals it's ready by writing a line to stdout.
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            stdout.read_line(&mut line).unwrap();
            if line.is_empty() {
                continue;
            }

            if let Some(port) = line.trim().strip_prefix("listening@") {
                log::info!("acked child startup: {}", line);
                return Virtual {
                    child,
                    port: port.parse().unwrap(),
                };
            }
        }
    }
    /// Sends `req` to this virtal RoT, using Cerberus-over-TCP.
    ///
    /// Blocks until a response comes back.
    pub fn send_cerberus<'a, Cmd>(
        &self,
        req: Cmd::Req,
        arena: &'a dyn Arena,
    ) -> Result<
        Result<Cmd::Resp, protocol::Error<'a, Cmd>>,
        server::Error<net::CerberusHeader>,
    >
    where
        Cmd: protocol::Command<'a, CommandType = cerberus::CommandType>,
    {
        tcp::send_cerberus::<Cmd>(self.port, req, arena)
    }

    /// Sends `req` to this virtal RoT, using SPDM-over-TCP.
    ///
    /// Blocks until a response comes back.
    pub fn send_spdm<'a, Cmd>(
        &self,
        req: Cmd::Req,
        arena: &'a dyn Arena,
    ) -> Result<
        Result<Cmd::Resp, protocol::Error<'a, Cmd>>,
        server::Error<net::SpdmHeader>,
    >
    where
        Cmd: protocol::Command<'a, CommandType = spdm::CommandType>,
    {
        tcp::send_spdm::<Cmd>(self.port, req, arena)
    }
}

/// Starts a server loop for serving PA-RoT requests, as described by `opts`.
pub fn serve(opts: Options) -> ! {
    log::info!("configuring server...");
    let networking = capabilities::Networking {
        max_message_size: opts.max_message_size,
        max_packet_size: opts.max_packet_size,
        mode: capabilities::RotMode::Platform,
        roles: capabilities::BusRole::Host.into(),
    };

    let timeouts = capabilities::Timeouts {
        regular: opts.regular_timeout,
        crypto: opts.crypto_timeout,
    };

    let identity = fakes::Identity::new(
        &opts.firmware_version,
        opts.vendor_firmware_versions
            .iter()
            .map(|(k, v)| (*k, v.as_slice())),
        &opts.unique_device_identity,
    );
    let reset = fakes::Reset::new(opts.resets_since_power_on);

    let mut hasher = ring::hash::Engine::new();
    let mut csrng = ring::csrng::Csrng::new();
    let mut ciphers = ring::sig::Ciphers::new();

    let trust_chain_bytes =
        opts.cert_chain.iter().map(Vec::as_ref).collect::<Vec<_>>();
    let mut signer = opts.alias_keypair.as_ref().map(|kp| match kp {
        KeyPairFormat::RsaPkcs8(pk8) => {
            match ring::rsa::Sign256::from_pkcs8(pk8) {
                Ok(rsa) => rsa,
                Err(e) => {
                    log::error!("could not parse alias keypair: {:?}", e);
                    std::process::exit(1);
                }
            }
        }
    });
    let mut trust_chain = cert::SimpleChain::<8>::parse(
        &trust_chain_bytes,
        opts.cert_format,
        &mut ciphers,
        signer.as_mut().map(|s| s as _),
    )
    .unwrap();
    let mut session = Session::new();

    let mut server = PaRot::new(manticore::server::pa_rot::Options {
        identity: &identity,
        reset: &reset,
        hasher: &mut hasher,
        csrng: &mut csrng,
        ciphers: &mut ciphers,
        trust_chain: &mut trust_chain,
        session: &mut session,
        pmr0: &opts.pmr0,
        device_id: opts.device_id,
        networking,
        timeouts,
    });

    match opts.protocol {
        Protocol::Cerberus => {
            let mut host = match TcpHostPort::<net::CerberusHeader>::bind() {
                Ok(host) => host,
                Err(e) => {
                    log::error!("could not connect to host: {:?}", e);
                    std::process::exit(1);
                }
            };
            let port = host.port();
            log::info!("bound to port {}", port);

            // Notify parent that we're listening.
            println!("listening@{}", port);

            let mut arena = BumpArena::new(vec![0; 1024]);

            log::info!("entering server loop");
            loop {
                if let Err(e) = server.process_request(&mut host, &arena) {
                    log::error!("failed to process request: {:?}", e);
                }
                arena.reset();
            }
        }
        Protocol::Spdm => {
            let mut host = match TcpHostPort::<net::SpdmHeader>::bind() {
                Ok(host) => host,
                Err(e) => {
                    log::error!("could not connect to host: {:?}", e);
                    std::process::exit(1);
                }
            };
            let port = host.port();
            log::info!("bound to port {}", port);

            // Notify parent that we're listening.
            println!("listening@{}", port);

            let mut arena = BumpArena::new(vec![0; 1024]);

            log::info!("entering server loop");
            loop {
                if let Err(e) = server.process_spdm_request(&mut host, &arena) {
                    log::error!("failed to process request: {:?}", e);
                }
                arena.reset();
            }
        }
    }
}
