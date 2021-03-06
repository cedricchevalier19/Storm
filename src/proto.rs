use actix;
use actix::{Actor, ActorContext, Arbiter, AsyncContext, Context, System};
use futures::{future, Future, Sink, Stream};
use mac_address;
use regex::RegexSetBuilder;
use tokio_codec::FramedRead;
use tokio_core;
use tokio_io::io::WriteHalf;
use tokio_io::AsyncRead;
use tokio_signal::unix::{Signal, SIGTERM};
use tokio_tcp::TcpStream;
use tokio_timer;

use codec;
use player;

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Command;
use std::time::{Duration, Instant};

pub struct Proto {
    sync_group_id: Option<String>,
    creation_time: Instant,
    stat_data: codec::StatData,
    server_ip: Ipv4Addr,
    name: String,
    output_device: player::AudioDevice,
    autostart: bool,
    player: actix::Addr<player::Player>,
    framed: actix::io::FramedWrite<WriteHalf<TcpStream>, codec::SlimCodec>,
}

impl Actor for Proto {
    type Context = Context<Self>;

    fn started(&mut self, _ctx: &mut Context<Self>) {
        let name = format!("ModelName={}", self.name);
        let caps = get_decode_caps();
        let player_caps: &[&str] = &[
            "Model=Storm",
            name.as_str(),
            "AccuratePlayPoints=1",
            "HasDigitalOut=1",
            "HasPolarityInversion=1",
        ];
        info!("Available docoders: {}", caps.join(","));

        let mut caps: Vec<String> = caps
            .iter()
            .cloned()
            .chain(player_caps.iter().cloned().map(|s| s.to_owned()))
            .collect();

        if let Some(ref sync_group) = self.sync_group_id {
            info!("Setting sync group: {}", sync_group);
            caps.push(format!("SyncgroupID={}", sync_group));
        }

        let mac = get_mac();
        info!("Using MAC address: {}", mac);

        let helo = codec::ClientMessage::Helo {
            device_id: 12,
            revision: 0,
            mac: mac,
            uuid: [0; 16],
            wlan_channel_list: 0,
            bytes_received: 0,
            capabilities: caps.join(","),
        };

        info!("Sending Helo");
        self.framed.write(helo);
    }

    fn stopping(&mut self, _ctx: &mut Context<Self>) -> actix::Running {
        info!("Sending Bye");
        self.framed.write(codec::ClientMessage::Bye(0));
        actix::Running::Stop
    }
}

impl actix::io::WriteHandler<io::Error> for Proto {}

impl actix::StreamHandler<codec::ServerMessage, io::Error> for Proto {
    fn handle(&mut self, msg: codec::ServerMessage, ctx: &mut Context<Self>) {
        match msg {
            codec::ServerMessage::Serv {
                ip_address,
                sync_group_id,
            } => {
                info!("Got serv message");
                spawn_proto(
                    ip_address,
                    sync_group_id,
                    self.name.as_str(),
                    Some(self.stat_data.buffer_size),
                    self.output_device.clone(),
                );
                ctx.stop();
            }

            codec::ServerMessage::Status(timestamp) => {
                info!("Got status request");
                self.stat_data.timestamp = timestamp;
                self.stat_data.jiffies = self.jiffies();
                self.framed.write(self.stat_data.make_stat_message("STMt"));
            }

            codec::ServerMessage::Stream {
                autostart,
                threshold,
                output_threshold,
                replay_gain,
                server_port,
                server_ip,
                http_headers,
            } => {
                info!("Got stream start");
                let bufsize = if self.stat_data.buffer_size > threshold {
                    self.stat_data.buffer_size
                } else {
                    threshold
                };
                self.stat_data.buffer_size = bufsize;
                self.stat_data.elapsed_milliseconds = 0;
                self.stat_data.elapsed_seconds = 0;
                self.stat_data.fullness = 0;
                self.stat_data.output_buffer_fullness = 0;
                self.stat_data.crlf = 0;
                self.autostart = autostart;
                self.framed.write(self.stat_data.make_stat_message("STMc"));
                self.player.do_send(player::PlayerControl::Stream {
                    autostart,
                    threshold: bufsize * 1024,
                    output_threshold,
                    replay_gain,
                    server_port,
                    server_ip,
                    control_ip: self.server_ip,
                    http_headers,
                })
            }

            codec::ServerMessage::Gain(gain_left, gain_right) => {
                info!("Got gain; Left: {}, Right: {}", gain_left, gain_right);
                self.player
                    .do_send(player::PlayerControl::Gain(gain_left, gain_right));
            }

            codec::ServerMessage::Enable(enable) => {
                info!("Got enable: {}", enable);
                self.player.do_send(player::PlayerControl::Enable(enable));
            }

            codec::ServerMessage::Stop => {
                info!("Got stream stop");
                self.player.do_send(player::PlayerControl::Stop);
            }

            codec::ServerMessage::Pause(millis) => {
                info!("Pause received with delay: {}", millis);
                if millis == 0 {
                    self.player.do_send(player::PlayerControl::Pause(false));
                } else {
                    self.player.do_send(player::PlayerControl::Pause(true));
                    let player_addr = self.player.clone();
                    Arbiter::spawn(
                        tokio_timer::Delay::new(
                            Instant::now() + Duration::from_millis(millis as u64),
                        )
                        .and_then(move |_| {
                            player_addr.do_send(player::PlayerControl::Unpause(true));
                            future::ok(())
                        })
                        .map_err(|_| ()),
                    )
                }
            }

            codec::ServerMessage::Unpause(millis) => {
                info!("Unpause received with delay: {}", millis);
                if millis == 0 {
                    self.player.do_send(player::PlayerControl::Unpause(false));
                } else {
                    let player_addr = self.player.clone();
                    let delay = millis - self.jiffies();
                    if delay > 0 {
                        Arbiter::spawn(
                            tokio_timer::Delay::new(
                                Instant::now() + Duration::from_millis(delay as u64),
                            )
                            .and_then(move |_| {
                                player_addr.do_send(player::PlayerControl::Unpause(true));
                                future::ok(())
                            })
                            .map_err(|_| ()),
                        )
                    } else {
                        player_addr.do_send(player::PlayerControl::Unpause(true));
                    }
                }
            }

            codec::ServerMessage::Skip(interval) => {
                info!("Skip ahead by: {}", interval);
                self.player.do_send(player::PlayerControl::Skip(interval));
            }

            codec::ServerMessage::Setname(name) => {
                info!("Setting player name to: {}", name);
                self.name = name;
            }

            codec::ServerMessage::Queryname => {
                self.framed
                    .write(codec::ClientMessage::Name(self.name.clone()));
            }

            codec::ServerMessage::Unknownsetd(id) => {
                warn!("Unused SETD id: {}", id);
            }

            codec::ServerMessage::Unrecognised(msg) => {
                warn!("Unrecognised message: {}", msg);
            }

            _ => (),
        }
    }
}

impl actix::Handler<player::PlayerMessages> for Proto {
    type Result = ();

    fn handle(&mut self, msg: player::PlayerMessages, ctx: &mut actix::Context<Self>) {
        match msg {
            player::PlayerMessages::Flushed => {
                self.framed.write(self.stat_data.make_stat_message("STMf"));
            }

            player::PlayerMessages::Paused => {
                self.framed.write(self.stat_data.make_stat_message("STMp"));
            }

            player::PlayerMessages::Unpaused => {
                self.framed.write(self.stat_data.make_stat_message("STMr"));
            }

            player::PlayerMessages::Eos => {
                self.framed.write(self.stat_data.make_stat_message("STMd"));
            }

            player::PlayerMessages::Established => {
                self.framed.write(self.stat_data.make_stat_message("STMe"));
            }

            player::PlayerMessages::Headers(crlf) => {
                self.stat_data.crlf = crlf;
                self.framed.write(self.stat_data.make_stat_message("STMh"));
            }

            player::PlayerMessages::Error => {
                self.framed.write(self.stat_data.make_stat_message("STMn"));
                // self.player.do_send(player::PlayerControl::Stop);
            }

            player::PlayerMessages::Start => {
                self.framed.write(self.stat_data.make_stat_message("STMs"));
                let proto = ctx.address().clone();
                Arbiter::spawn(
                    tokio_timer::Delay::new(Instant::now() + Duration::from_millis(400))
                        .and_then(move |_| {
                            proto.do_send(player::PlayerMessages::Sendstatus);
                            future::ok(())
                        })
                        .map_err(|_| ()),
                )
            }

            player::PlayerMessages::Streamdata {
                position,
                fullness,
                output_buffer_fullness,
            } => {
                self.stat_data.elapsed_milliseconds = position as u32;
                self.stat_data.elapsed_seconds = position as u32 / 1000;
                self.stat_data.fullness = fullness;
                self.stat_data.output_buffer_fullness = output_buffer_fullness;
            }

            player::PlayerMessages::Bufsize(buf_size) => {
                self.stat_data.bytes_received =
                    self.stat_data.bytes_received.wrapping_add(buf_size as u64);
            }

            player::PlayerMessages::Sendstatus => {
                self.stat_data.jiffies = self.jiffies();
                self.framed.write(self.stat_data.make_stat_message("STMt"));
            }

            player::PlayerMessages::Overrun => {
                if !self.autostart {
                    self.player.do_send(player::PlayerControl::Pause(true));
                    self.framed.write(self.stat_data.make_stat_message("STMl"));
                    self.autostart = true;
                }
            }
        }
    }
}

impl Proto {
    fn jiffies(&self) -> u32 {
        let dur = self.creation_time.elapsed();
        ((dur.as_secs() * 1000 + dur.subsec_millis() as u64) % (::std::u32::MAX as u64 + 1)) as u32
    }
}

pub fn run(
    server_ip: Ipv4Addr,
    sync_group: Option<String>,
    name: &str,
    bufsize: Option<u32>,
    output_device: player::AudioDevice,
) -> std::io::Result<()> {
    let sys = System::new("Storm");
    spawn_proto(server_ip, sync_group, name, bufsize, output_device);
    spawn_signal_handler();
    sys.run()
}

fn spawn_proto(
    server_ip: Ipv4Addr,
    sync_group: Option<String>,
    name: &str,
    bufsize: Option<u32>,
    output_device: player::AudioDevice,
) {
    let name = name.to_owned();
    let addr = SocketAddr::new(IpAddr::V4(server_ip), 3483);
    Arbiter::spawn(
        TcpStream::connect(&addr)
            .and_then(move |stream| {
                Proto::create(move |ctx| {
                    let player = player::Player::new(ctx.address(), output_device.clone());
                    let (r, w) = stream.split();
                    ctx.add_stream(FramedRead::new(r, codec::SlimCodec));
                    let mut proto = Proto {
                        sync_group_id: sync_group,
                        creation_time: Instant::now(),
                        stat_data: codec::StatData::default(),
                        server_ip: server_ip,
                        name: name,
                        output_device: output_device,
                        autostart: true,
                        player: player.start(),
                        framed: actix::io::FramedWrite::new(w, codec::SlimCodec, ctx),
                    };
                    proto.stat_data.buffer_size = bufsize.unwrap_or(0);
                    proto
                });
                future::ok(())
            })
            .map_err(|e| {
                error!("Cannot connect to server: {}", e);
                ::std::process::exit(2)
            }),
    );
}

fn spawn_signal_handler() {
    Arbiter::spawn(
        Signal::new(SIGTERM)
            .flatten_stream()
            .into_future()
            .then(|_| {
                info!("Received TERM signal, exiting");
                System::current().stop();
                future::ok(())
            }),
    );
}

struct Discover;

impl tokio_core::net::UdpCodec for Discover {
    type In = Ipv4Addr;
    type Out = char;

    fn decode(&mut self, src: &SocketAddr, _buf: &[u8]) -> io::Result<Self::In> {
        if let SocketAddr::V4(addr) = src {
            Ok(*addr.ip())
        } else {
            unreachable!()
        }
    }

    fn encode(&mut self, msg: Self::Out, buf: &mut Vec<u8>) -> SocketAddr {
        buf.push(msg as u8);
        "255.255.255.255:3483".parse().unwrap()
    }
}

pub fn discover() -> io::Result<Ipv4Addr> {
    let mut core = tokio_core::reactor::Core::new()?;
    let handle = core.handle();

    let sock = tokio_core::net::UdpSocket::bind(&"0.0.0.0:0".parse().unwrap(), &handle)?;
    sock.set_broadcast(true)?;

    let (discover_out, discover_in) = sock.framed(Discover).split();

    info!("Looking for server ...");

    let pings = tokio_timer::Interval::new(Instant::now(), Duration::from_secs(5))
        .map(|_| 'e')
        .map_err(|_| ());
    let pinger = discover_out
        .sink_map_err(|_| ())
        .send_all(pings)
        .map(|_| ())
        .map_err(|_| ());
    handle.spawn(pinger);

    let discovery = discover_in.take(1).into_future();
    match core.run(discovery).map_err(|(e, _)| e) {
        Ok((Some(addr), _)) => {
            info!("Found server at {}", addr);
            Ok(addr)
        }
        Err(e) => Err(e),
        _ => unreachable!(),
    }
}

fn get_mac() -> mac_address::MacAddress {
    match mac_address::get_mac_address() {
        Ok(Some(mac)) => mac,
        _ => mac_address::MacAddress::new([1, 2, 3, 4, 5, 6]),
    }
}

fn get_decode_caps() -> Vec<String> {
    let ffmpeg = match Command::new("/usr/bin/ffmpeg").arg("-decoders").output() {
        Ok(output) => output.stdout,
        Err(_) => {
            warn!("No decoders detected");
            Vec::new()
        }
    };

    let decoders = vec![
        ("alac", "alc"),
        ("wma", "wma"),
        ("wmap", "wmap"),
        ("wmal", "wmal"),
        ("flac", "flc"),
        ("aac", "aac"),
        ("vorbis", "ogg"),
        ("pcm", "pcm"),
        ("mp3", "mp3"),
    ];

    let sets: Vec<String> = decoders
        .iter()
        .map(|s| [r"^.A.....\s+", s.0].join(""))
        .collect();

    let matches: Vec<_> = match RegexSetBuilder::new(&sets).multi_line(true).build() {
        Ok(set) => set,
        Err(_) => return Vec::new(),
    }
    .matches(std::str::from_utf8(&ffmpeg).unwrap())
    .into_iter()
    .collect();

    let mut caps = Vec::new();
    matches
        .iter()
        .for_each(|cap| caps.push(decoders[*cap].1.to_owned()));
    caps
}
